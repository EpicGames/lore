// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::PartialEq;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::allocator::GrowVec;
use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;
use tokio::sync::Mutex;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::Address;
use crate::Hash;
use crate::Partition;
use crate::errors::AddressNotFound;
use crate::immutable_store::ImmutableStore;
use crate::immutable_store::StoreError;
use crate::local::immutable_store::SerializeFailureGuard;
use crate::local::immutable_store::format_bucket_path;
use crate::store_types::KeyType;
use crate::store_types::KeyValueStream;

#[error_set]
pub enum LocalMutableStoreError {}

pub const GROUP_COUNT: usize = 256;
pub const BUCKET_COUNT: usize = 256;

pub const DEFAULT_FLUSH_DELAY_SECONDS: u64 = 0;

/// Configuration for `LocalMutableStore`. Defaults are client-favoring (level 1, threshold 1000),
/// matching `ImmutableStoreSettings::default()`. Server processes that want today's flat 256-bucket
/// layout should set `initial_fan_out_level = 256` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutableStoreSettings {
    /// Background flush delay; `0` means flush immediately.
    pub flush_delay_seconds: u64,
    /// Number of buckets per group at store creation. Must be a value from
    /// `lore_storage::local::fan_out::LEVEL_LADDER`. Existing on-disk stores ignore this and load
    /// at whatever level their marker files indicate (or 256 for legacy stores with no marker).
    pub initial_fan_out_level: usize,
    /// Per-bucket entry threshold that triggers fan-out at the next serialize. Default is `1000`.
    pub fan_out_threshold: usize,
    /// Source of truth (server) rather than a cache. When `true`, a corrupt bucket is a hard
    /// error; when `false`, it is reset to empty since its entries repopulate on next sync.
    pub authoritative: bool,
}

impl Default for MutableStoreSettings {
    fn default() -> Self {
        Self {
            flush_delay_seconds: DEFAULT_FLUSH_DELAY_SECONDS,
            initial_fan_out_level: 1,
            fan_out_threshold: crate::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
            authoritative: false,
        }
    }
}

// 32 u32 makes the u32 growvec chunks 256 bytes in size
const CHUNK_SIZE_U32: usize = 32;

// 8 entries makes the MutableStoreEntry growvec chunks 640 bytes in size
const CHUNK_SIZE_ENTRY: usize = 8;

struct Key(Hash);

impl Key {
    fn make_typed(mut hash: Hash, key: KeyType) -> Key {
        hash.data_mut()[2] = key as u8;
        Key(hash)
    }

    fn hash(&self) -> Hash {
        self.0
    }

    fn group_index(&self) -> usize {
        self.0.data()[0] as usize
    }

    fn key_type_from_hash(other: &Hash) -> u8 {
        other.data()[2]
    }
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, FromBytes, Immutable)]
pub struct MutableStoreEntry {
    /// Partition for which the key-value tuple is associated
    pub partition: Partition,
    /// Key, where on newer serialized versions of the store it is a `Key`
    pub key: Hash,
    /// Value, usually a data blob hash, but can be anything 32 bytes or less
    pub value: Hash,
}

#[derive(Default)]
pub struct MutableStoreBucket {
    pub entry: GrowVec<MutableStoreEntry, CHUNK_SIZE_ENTRY>,
    pub sorted_index: GrowVec<u32, CHUNK_SIZE_U32>,
    flush: Option<JoinHandle<()>>,
    deserialized: bool,
    pub version: u32,
    serialize_lock: Arc<Mutex<()>>,
}

pub struct MutableStoreGroup {
    /// Per-slot lazily-initialized bucket. Empty `OnceLock` at construction; first
    /// `bucket()` call materializes the `Arc<RwLock<MutableStoreBucket>>`. Use
    /// `try_bucket()` for paths that must be a no-op when the slot has never been
    /// touched (flush of a clean slot, dirty-only scans).
    pub bucket: [OnceLock<Arc<RwLock<MutableStoreBucket>>>; BUCKET_COUNT],
    /// Dirty flag per bucket, kept outside the bucket's `RwLock` so `flush_all`
    /// can scan for work with lock-free atomic loads.
    pub dirty: [AtomicBool; BUCKET_COUNT],
    /// Number of active buckets in this group. Slots `[0..bucket_count]` are addressable;
    /// `[bucket_count..BUCKET_COUNT]` are pre-allocated but unused (always empty, never dirty,
    /// never serialized). Loaded with `Relaxed` ordering — synchronization between fan-out and
    /// concurrent reads/writes comes from the per-bucket `RwLock`, not this atomic.
    pub bucket_count: std::sync::atomic::AtomicUsize,
    /// Version to write into bucket file headers on serialize. `LazyFanOut` (v3) for fan-out-aware
    /// stores; `TypedItems` (v2) for legacy stores untouched by fan-out-aware code (preserves
    /// backward compatibility with older clients). Set once at store construction; same value for
    /// every group in the same store. `Relaxed` ordering — only read by serialize.
    pub serialize_version: std::sync::atomic::AtomicU32,
    /// Per-bucket entry threshold that triggers a fan-out at the next serialize. Mirrored from
    /// `MutableStoreSettings::fan_out_threshold` so the per-group serialize task has access
    /// without holding a store reference. Same value across all groups in a store.
    pub fan_out_threshold: usize,
    /// Bucket count recorded by the on-disk `level` marker. `0` means "no marker exists yet"
    /// (a fresh fan-out-aware store before its first flush). Updated only after a successful
    /// two-phase commit (`level.pending` deleted), so a mismatch with `bucket_count` indicates a
    /// pending level transition that needs the two-phase commit on the next flush.
    pub committed_level: std::sync::atomic::AtomicUsize,
}

impl MutableStoreGroup {
    /// Resolve a bucket slot, creating its `Arc<RwLock<MutableStoreBucket>>` on
    /// first touch.
    #[inline]
    pub fn bucket(&self, idx: usize) -> &Arc<RwLock<MutableStoreBucket>> {
        self.bucket[idx].get_or_init(|| Arc::new(RwLock::new(MutableStoreBucket::default())))
    }

    /// Return the bucket at `idx` only if it has been initialized. Never
    /// triggers materialization.
    #[inline]
    pub fn try_bucket(&self, idx: usize) -> Option<&Arc<RwLock<MutableStoreBucket>>> {
        self.bucket[idx].get()
    }
}

pub struct LocalMutableStore {
    pub path: Option<Arc<PathBuf>>,
    pub group: Vec<Arc<MutableStoreGroup>>,
    pub flush_delay_seconds: u64,
    pub needs_upgrade: AtomicBool,
    pub authoritative: bool,

    // This field must be dropped last so it must be declared last
    #[allow(dead_code)]
    pub lock: Option<FSLock>,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MutableStoreVersion {
    /// Initial version
    Initial = 1,
    /// Typed items
    TypedItems = 2,
    /// Lazy fan-out: bucket count per group is variable (see `local::fan_out`); marker file may
    /// be present in the group directory recording the current bucket count. Bucket file format
    /// itself is unchanged from `TypedItems`; this version is purely a forward-compatibility
    /// sentinel that prevents older binaries from misinterpreting `index_<bb>` filenames.
    LazyFanOut = 3,
}

#[repr(C)]
#[derive(Default, IntoBytes, FromBytes, Immutable)]
struct MutableStoreHeader {
    version: u32,
    _unused: u32,
    count: u32,
    _unused_two: u32,
    // Following the index store is
    // Sorted index of entries
    // sorted_index: [u32; count]
    // All entries
    // entry[MutableStoreEntry; count]
}

/// Classification of a failed bucket parse. A `FutureVersion` file must propagate untouched;
/// deleting it would destroy data written by a newer binary. A `Corrupt` file is safe to reset.
enum DeserializeFileError {
    FutureVersion(u32),
    Corrupt(String),
}

/// Final-destination segments for a bucket read: one vectored operation
/// scatters the on-disk sorted-index and entry regions straight into the
/// bucket's chunk allocations, with no staging buffer.
struct MutableBucketSegments {
    sorted_index: GrowVec<u32, CHUNK_SIZE_U32>,
    entry: GrowVec<MutableStoreEntry, CHUNK_SIZE_ENTRY>,
}

impl lore_io::StableBufListMut for MutableBucketSegments {
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
struct MutableBucketWriteSegments {
    /// The serialized header, in a fixed-size allocation rather than a `Vec`: its length is a
    /// compile-time constant. It stays behind a pointer because [`lore_io::StableBufList`]
    /// requires a segment to keep its address when the value moves, and the ring backend moves
    /// the segment list into its operation entry after taking the pointers.
    header: Box<[u8; size_of::<MutableStoreHeader>()]>,
    bucket: OwnedRwLockReadGuard<MutableStoreBucket, MutableStoreBucket>,
}

impl lore_io::StableBufList for MutableBucketWriteSegments {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.header.as_ref().as_slice())
            .chain(self.bucket.sorted_index.byte_segments())
            .chain(self.bucket.entry.byte_segments())
    }
}

impl MutableStoreBucket {
    async fn deserialize_files(
        path: PathBuf,
        authoritative: bool,
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<MutableStoreEntry, CHUNK_SIZE_ENTRY>,
            u32,
        ),
        LocalMutableStoreError,
    > {
        let latest_version = MutableStoreVersion::LazyFanOut as u32;

        let (file, metadata, head) = match lore_io::IoDriver::global()
            .open_read_head(
                &path,
                &lore_io::OpenOptions::new().read(true),
                crate::local::immutable_store::BUCKET_HEAD_READ,
            )
            .await
        {
            Ok(parts) => parts,
            Err(err) => {
                if err.kind() == ErrorKind::NotFound {
                    return Ok((GrowVec::new(), GrowVec::new(), latest_version));
                }
                return Err(LocalMutableStoreError::internal_with_context(
                    err,
                    "opening mutable store bucket file",
                ));
            }
        };

        match Self::deserialize_file_content(&file, metadata.len() as usize, &head, latest_version)
            .await
        {
            Ok(result) => Ok(result),
            Err(DeserializeFileError::FutureVersion(_version)) => {
                Err(LocalMutableStoreError::internal_with_context(
                    io::Error::other(
                        "Incompatible store version encountered, please update your client to the latest version",
                    ),
                    "Failed to deserialize storage bucket",
                ))
            }
            // Authoritative store: the bucket is the only copy, so fail loud and keep the file.
            Err(DeserializeFileError::Corrupt(reason)) if authoritative => {
                Err(LocalMutableStoreError::internal(format!(
                    "corrupt mutable store bucket {}: {reason}",
                    path.display()
                )))
            }
            Err(DeserializeFileError::Corrupt(reason)) => {
                Self::recover_corrupt_bucket(&path, reason, latest_version).await
            }
        }
    }

    /// Buckets that fit inside the composite open's head bytes — the common
    /// case — parse straight from the head with no further dispatch. Larger
    /// buckets do one vectored read scattering the sorted index and entries
    /// straight into their final chunk allocations.
    async fn deserialize_file_content(
        file: &lore_io::IoFile,
        file_size: usize,
        head: &[u8],
        latest_version: u32,
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<MutableStoreEntry, CHUNK_SIZE_ENTRY>,
            u32,
        ),
        DeserializeFileError,
    > {
        // Guard against underflow before the count subtraction below. The head covers the whole
        // file when it is smaller than the head length, so a head too short for the header is a
        // file too short for one.
        let header_size = size_of::<MutableStoreHeader>();
        if head.len() < header_size {
            return Err(DeserializeFileError::Corrupt(format!(
                "file size {file_size} smaller than header size {header_size}"
            )));
        }
        let expected_count =
            (file_size - header_size) / (size_of::<u32>() + size_of::<MutableStoreEntry>());
        if expected_count == 0 {
            return Ok((GrowVec::new(), GrowVec::new(), latest_version));
        }

        let mut header = MutableStoreHeader::new_zeroed();
        header.as_mut_bytes().copy_from_slice(&head[..header_size]);

        if (header.version > latest_version) && (header.version < 0xFFFF) {
            return Err(DeserializeFileError::FutureVersion(header.version));
        }

        if header.count != expected_count as u32 {
            return Err(DeserializeFileError::Corrupt(format!(
                "mutable store bucket header has invalid count {} when expecting {expected_count}",
                header.count,
            )));
        }

        if file_size <= head.len() {
            let mut reader = &head[header_size..];
            let sorted_index = GrowVec::read_from(&mut reader, expected_count).map_err(|err| {
                DeserializeFileError::Corrupt(format!("read sorted index: {err}"))
            })?;
            let entry = GrowVec::read_from(&mut reader, expected_count)
                .map_err(|err| DeserializeFileError::Corrupt(format!("read entries: {err}")))?;
            return Ok((sorted_index, entry, header.version));
        }

        let segments = MutableBucketSegments {
            // SAFETY: the scatter below fills every byte of both vectors or fails, and a
            // failure drops them here rather than returning them.
            sorted_index: unsafe { GrowVec::new_unzeroed_with_size(expected_count) },
            entry: unsafe { GrowVec::new_unzeroed_with_size(expected_count) },
        };
        let segments = file
            .read_exact_vectored_at(segments, header_size as u64)
            .await
            .map_err(|err| DeserializeFileError::Corrupt(format!("read bucket data: {err}")))?;

        Ok((segments.sorted_index, segments.entry, header.version))
    }

    /// Drop the corrupt file and return an empty bucket. The lost entries repopulate from the
    /// immutable store or remote on the next sync.
    async fn recover_corrupt_bucket(
        path: &Path,
        reason: String,
        latest_version: u32,
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<MutableStoreEntry, CHUNK_SIZE_ENTRY>,
            u32,
        ),
        LocalMutableStoreError,
    > {
        lore_base::lore_warn!(
            "Resetting corrupt mutable bucket {} after deserialize failure: {reason}. Bucket lookup state lost; entries repopulate from the immutable store / remote on next sync.",
            path.display()
        );
        if let Err(err) = lore_io::IoDriver::global().remove_file(path).await
            && err.kind() != ErrorKind::NotFound
        {
            return Err(LocalMutableStoreError::internal_with_context(
                err,
                "Failed to remove corrupt mutable store bucket",
            ));
        }
        Ok((GrowVec::new(), GrowVec::new(), latest_version))
    }

    pub async fn deserialize(
        &mut self,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        _epoch_reset: bool,
        authoritative: bool,
    ) -> Result<(), LocalMutableStoreError> {
        if self.deserialized {
            return Ok(());
        }

        // Ensure only one serialization/deserialization of this bucket is happening at any given time
        let _lock = self.serialize_lock.lock().await;

        if self.deserialized {
            return Ok(());
        }

        let path = format_bucket_path(path, group_index, bucket_index);

        let (sorted_index, entry, version) = Self::deserialize_files(path, authoritative).await?;

        self.sorted_index = sorted_index;
        self.entry = entry;
        self.version = version;
        self.deserialized = true;

        Ok(())
    }

    async fn serialize_files(
        bucket: OwnedRwLockReadGuard<MutableStoreBucket, MutableStoreBucket>,
        group: Arc<MutableStoreGroup>,
        bucket_index: usize,
        path: PathBuf,
        sync_data: bool,
    ) -> Result<(), LocalMutableStoreError> {
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
            let _ = lore_io::IoDriver::global()
                .create_dir_all(parent_path)
                .await;
        }

        let count = bucket.entry.len();
        if bucket.sorted_index.len() != count {
            return Err(LocalMutableStoreError::internal(
                "mutable store bucket entry and index count mismatch",
            ));
        }

        let mut header = MutableStoreHeader::new_zeroed();
        header.version = group.serialize_version.load(atomic::Ordering::Relaxed);
        header.count = count as u32;

        let segments = MutableBucketWriteSegments {
            header: {
                let mut bytes = Box::new([0u8; size_of::<MutableStoreHeader>()]);
                bytes.copy_from_slice(header.as_bytes());
                bytes
            },
            bucket,
        };

        let file_options = lore_io::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true);
        if let Some(mut guard) = temporary_guard.take() {
            lore_io::IoDriver::global()
                .write_file_segments_atomic(&temporary_path, &path, &file_options, segments)
                .await
                .internal("writing mutable store bucket")?;

            guard.success();
        } else {
            lore_io::IoDriver::global()
                .write_file_segments(&path, &file_options, segments, false)
                .await
                .internal("writing mutable store bucket")?;
        }

        Ok(())
    }

    pub async fn serialize(
        bucket: OwnedRwLockReadGuard<MutableStoreBucket, MutableStoreBucket>,
        group: Arc<MutableStoreGroup>,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        sync_data: bool,
    ) -> Result<(), LocalMutableStoreError> {
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

        lore_base::lore_trace!("Serialize mutable store group {group_index} bucket {bucket_index}");

        let path = format_bucket_path(path, group_index, bucket_index);

        Self::serialize_files(bucket, group, bucket_index, path, sync_data).await
    }

    /// Serialize the bucket to its `.new` twin during a fan-out commit. Differs from the regular
    /// `serialize` path in two ways: (1) bypasses the `count == 0` early-exit and the
    /// `dirty.swap(false) → skip-if-was-false` short-circuit, because every `[0..committed_level]`
    /// bucket must be rewritten at the new layout to overwrite stale level-N files even if it's
    /// empty post-redistribute; (2) always clears dirty after claiming ownership. The clear is
    /// safe because the caller holds the bucket's read lock — no concurrent writer can set
    /// dirty=true while we hold it, so any post-release write will correctly re-set dirty and
    /// be picked up by the next flush, matching the regular `serialize` path's semantics.
    ///
    /// Reuses `serialize_files` internally; with `sync_data = true`, the path becomes
    /// `index_<bb>.new.tmp` → atomic rename → `index_<bb>.new`.
    pub async fn serialize_to_new(
        bucket: OwnedRwLockReadGuard<MutableStoreBucket, MutableStoreBucket>,
        group: Arc<MutableStoreGroup>,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        sync_data: bool,
    ) -> Result<(), LocalMutableStoreError> {
        let _lock = bucket.serialize_lock.clone().lock_owned().await;

        // Claim ownership of the bucket's current content. We hold the bucket's read lock so no concurrent writer can have set dirty between the time we decided to serialize and now.
        group.dirty[bucket_index].swap(false, atomic::Ordering::Relaxed);

        let final_path = format_bucket_path(path, group_index, bucket_index);
        let new_path = {
            let mut p = final_path.into_os_string();
            p.push(crate::local::fan_out::BUCKET_NEW_SUFFIX);
            PathBuf::from(p)
        };

        Self::serialize_files(bucket, group, bucket_index, new_path, sync_data).await
    }

    pub fn lookup(&self, partition: Partition, key: Hash) -> (Hash, bool, usize) {
        let count = self.entry.len();
        let mut start = 0;
        let mut end = count;

        // Binary search the bucket
        while start < end {
            let slot = (start + end) / 2;
            let entry_index = self.sorted_index[slot] as usize;
            let entry = &self.entry[entry_index];
            // This two step memory compare performs a full compare of the combined
            // hash-partition doublet data as well as keeping track of the best matching slot
            let mut order = key.cmp(&entry.key);
            if order == std::cmp::Ordering::Equal {
                order = partition.cmp(&entry.partition);
                if order == std::cmp::Ordering::Equal {
                    return (entry.value, true, slot);
                }
            }

            if order == std::cmp::Ordering::Less {
                end = slot;
            } else {
                start = slot + 1;
            }
        }

        (Hash::default(), false, start)
    }

    /// Binary search the sorted index range `[lo, hi)` for any entry whose key has
    /// `data()[2] == key_type as u8`. Sound only when the entries in `[lo, hi)` share
    /// `data()[0]` AND `data()[1]` so the full-hash sort order collapses to a `data()[2..]`
    /// sort within the range; callers must restrict the range accordingly (e.g. via
    /// `upper_bound_bucket_byte`). Returns `(true, slot)` on a match or `(false, start)` if no
    /// entry in the range has the requested key type.
    pub fn lookup_any_with_key_type_in_range(
        &self,
        key_type: KeyType,
        lo: usize,
        hi: usize,
    ) -> (bool, usize) {
        let mut start = lo;
        let mut end = hi;

        let key_type_byte = key_type as u8;
        while start < end {
            let slot = (start + end) / 2;
            let entry_index = self.sorted_index[slot] as usize;
            let entry = &self.entry[entry_index];

            let order = key_type_byte.cmp(&Key::key_type_from_hash(&entry.key));
            match order {
                std::cmp::Ordering::Less => end = slot,
                std::cmp::Ordering::Greater => start = slot + 1,
                std::cmp::Ordering::Equal => return (true, slot),
            }
        }

        (false, start)
    }

    /// Binary-search the sorted index range `[lo, hi)` for the first slot whose entry has a
    /// bucket byte greater than `bucket_byte`. The bucket byte is the second hash byte
    /// (`data()[1]`); its top `log2(N)` bits select the bucket at fan-out level `N`, and at
    /// level 256 the full byte equals the bucket index. Within a single bucket at lower
    /// fan-out levels, `sorted_index` orders entries by this byte primarily and key-type
    /// secondarily, so this function carves the bucket's `sorted_index` into one slice
    /// per distinct bucket-byte value — exactly the slices on which
    /// `lookup_any_with_key_type_in_range` is sound.
    pub fn upper_bound_bucket_byte(&self, lo: usize, hi: usize, bucket_byte: u8) -> usize {
        let mut start = lo;
        let mut end = hi;
        while start < end {
            let mid = (start + end) / 2;
            let entry_index = self.sorted_index[mid] as usize;
            let entry = &self.entry[entry_index];
            if entry.key.data()[1] <= bucket_byte {
                start = mid + 1;
            } else {
                end = mid;
            }
        }
        start
    }

    pub fn test_inject(&mut self, partition: Partition, key: Hash, value: Hash) {
        let (existing_value, match_made, insert_slot) = self.lookup(partition, key);

        if match_made {
            // Previous entry found
            if existing_value == value {
                return;
            }
            let entry_index = self.sorted_index[insert_slot] as usize;
            self.entry[entry_index].value = value;
        } else {
            if value.is_zero() {
                return;
            }

            // inject new entry
            let count = self.entry.len();
            self.sorted_index.insert(insert_slot, count as u32);

            self.entry.push(MutableStoreEntry {
                key,
                partition,
                value,
            });
        }
    }
}

impl LocalMutableStore {
    pub async fn new(
        path: Option<impl AsRef<Path>>,
        settings: MutableStoreSettings,
        _immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<Self, LocalMutableStoreError> {
        let flush_delay_seconds = settings.flush_delay_seconds;
        let authoritative = settings.authoritative;
        let mutable_path = path.as_ref().map(|path| {
            let mut path = path.as_ref().to_path_buf();
            path.push("mutable");
            Arc::new(path)
        });

        let mut needs_upgrade = false;
        let mut version = MutableStoreVersion::Initial;
        let lock = if let Some(path) = mutable_path.as_deref() {
            if !path.exists() {
                let _ = lore_io::IoDriver::global()
                    .create_dir_all(path.as_path())
                    .await;
            }
            let lock = FSLock::acquire_directory_lock(path.as_path())
                .await
                .internal("acquiring mutable store lock")?;

            let index_existed = std::fs::exists(path.join("index")).unwrap_or_default();

            // Check store version
            let version_path = path.join("version");
            if let Ok(bytes) = lore_io::IoDriver::global()
                .read_file_bytes(&version_path)
                .await
            {
                let stored = bytes
                    .as_ref()
                    .get(..4)
                    .map(|value| u32::from_ne_bytes(value.try_into().expect("4 bytes")))
                    .unwrap_or_default();
                match stored {
                    x if x == MutableStoreVersion::LazyFanOut as u32 => {
                        version = MutableStoreVersion::LazyFanOut;
                    }
                    x if x == MutableStoreVersion::TypedItems as u32 => {
                        version = MutableStoreVersion::TypedItems;
                    }
                    _ => {
                        lore_base::lore_debug!("Mutable store NOT at latest version: {version:?}");
                    }
                }
            };

            if version == MutableStoreVersion::Initial {
                // Pre-existing stores need migration (defer until remote is
                // available); brand new stores write LazyFanOut directly.
                let stored_version = if index_existed {
                    needs_upgrade = true;
                    version as u32
                } else {
                    MutableStoreVersion::LazyFanOut as u32
                };
                lore_io::IoDriver::global()
                    .write_file_bytes(
                        &version_path,
                        bytes::Bytes::copy_from_slice(&stored_version.to_ne_bytes()),
                        false,
                    )
                    .await
                    .map_err(|err| {
                        LocalMutableStoreError::internal_with_context(
                            err,
                            "Failed to upgrade mutable store",
                        )
                    })?;
            }
            Some(lock)
        } else {
            None
        };

        // Per-group level marker detection. For each group dir (if present on disk), first run
        // T10 recovery to roll forward any interrupted fan-out commit, then read the marker; if
        // the marker is missing, fall back to `settings.initial_fan_out_level` for fresh stores
        // or 256 for existing legacy stores (the pre-fan-out 256-bucket layout). `committed_level`
        // tracks the on-disk marker value (0 if absent) for the flush path's two-phase decision.
        let index_existed_on_disk = mutable_path
            .as_ref()
            .is_some_and(|p| p.join("index").exists());
        // Every group is read at once, for the reasons the immutable store's open records:
        // awaiting the groups in turn puts a store open behind `GROUP_COUNT` round trips to the
        // I/O engine, and each task carries the group it answers for because completions arrive
        // in whatever order the reads finish.
        let initial_fan_out_level = settings.initial_fan_out_level;
        let mut levels = vec![(initial_fan_out_level, 0usize, false); GROUP_COUNT];
        if let Some(path) = mutable_path.as_ref() {
            let index_path = path.join("index");
            let mut tasks = JoinSet::new();
            for group_index in 0..GROUP_COUNT {
                let group_path = index_path.join(format!("{:02x}", group_index as u8));
                lore_base::lore_spawn!(tasks, async move {
                    // Roll forward any pending fan-out commit before reading the marker. After this returns the marker reflects the post-recovery state.
                    if group_path.exists()
                        && let Err(err) =
                            crate::local::fan_out::recover_level_transition(&group_path, false)
                                .await
                    {
                        return (
                            group_index,
                            Err(LocalMutableStoreError::internal_with_context(
                                err,
                                "Failed to recover pending level transition for group",
                            )),
                        );
                    }

                    let level = match crate::local::fan_out::read_level_marker(&group_path).await {
                        Ok(Some(level)) => Ok((level, level, true)),
                        Ok(None) if index_existed_on_disk => Ok((BUCKET_COUNT, 0, false)),
                        Ok(None) => Ok((initial_fan_out_level, 0, false)),
                        Err(err) => Err(LocalMutableStoreError::internal_with_context(
                            err,
                            "Failed to read level marker for group",
                        )),
                    };
                    (group_index, level)
                });
            }

            while let Some(joined) = tasks.join_next().await {
                let (group_index, level) = joined.map_err(|err| {
                    LocalMutableStoreError::internal_with_context(err, "level marker task")
                })?;
                levels[group_index] = level?;
            }
        }

        let mut bucket_counts: Vec<usize> = Vec::with_capacity(GROUP_COUNT);
        let mut committed_levels: Vec<usize> = Vec::with_capacity(GROUP_COUNT);
        let mut any_marker_seen = false;
        for (initial, committed, marker_seen) in levels {
            bucket_counts.push(initial);
            committed_levels.push(committed);
            any_marker_seen |= marker_seen;
        }

        // Determine serialize_version per Decision 8. Fresh stores and stores with markers / older
        // versions becoming fan-out-aware all go to LazyFanOut. Existing TypedItems stores with no
        // markers stay at TypedItems for backward compatibility.
        let serialize_version: u32 = if !index_existed_on_disk
            || any_marker_seen
            || version != MutableStoreVersion::TypedItems
        {
            MutableStoreVersion::LazyFanOut as u32
        } else {
            MutableStoreVersion::TypedItems as u32
        };

        let mut store = LocalMutableStore {
            path: mutable_path,
            lock,
            group: Vec::with_capacity(GROUP_COUNT),
            flush_delay_seconds,
            needs_upgrade: AtomicBool::new(needs_upgrade),
            authoritative,
        };

        for (group_index, &count) in bucket_counts.iter().enumerate() {
            store.group.push(Arc::new(MutableStoreGroup {
                bucket: [const { OnceLock::new() }; BUCKET_COUNT],
                dirty: std::array::from_fn(|_| AtomicBool::new(false)),
                bucket_count: std::sync::atomic::AtomicUsize::new(count),
                serialize_version: std::sync::atomic::AtomicU32::new(serialize_version),
                fan_out_threshold: settings.fan_out_threshold,
                committed_level: std::sync::atomic::AtomicUsize::new(committed_levels[group_index]),
            }));
        }

        Ok(store)
    }

    pub fn needs_upgrade(&self) -> bool {
        self.needs_upgrade.load(atomic::Ordering::Relaxed)
    }

    pub fn group(&self, index: usize) -> Arc<MutableStoreGroup> {
        self.group[index].clone()
    }

    fn mark_dirty(
        self: Arc<Self>,
        bucket: &mut MutableStoreBucket,
        group_index: usize,
        bucket_index: usize,
    ) {
        let was_dirty =
            self.group[group_index].dirty[bucket_index].swap(true, atomic::Ordering::Relaxed);
        if !was_dirty {
            if let Some(flush_task) = bucket.flush.as_ref()
                && flush_task.is_finished()
            {
                let _ = bucket.flush.take();
            }

            if bucket.flush.is_none() && self.flush_delay_seconds > 0 {
                let weak_self = Arc::downgrade(&self);
                bucket.flush = Some(Self::flush_delayed(
                    weak_self,
                    group_index,
                    bucket_index,
                    self.flush_delay_seconds,
                ));
            }
        }
    }

    fn flush_delayed(
        weak_ref: Weak<LocalMutableStore>,
        group_index: usize,
        bucket_index: usize,
        delay: u64,
    ) -> JoinHandle<()> {
        lore_base::lore_spawn!(async move {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            if let Some(store) = weak_ref.upgrade()
                && let Some(path) = store.path.as_ref()
            {
                let group = store.group[group_index].clone();
                let Some(bucket) = group.try_bucket(bucket_index).cloned() else {
                    return;
                };

                let bucket = bucket.read_owned().await;
                let _ = MutableStoreBucket::serialize(
                    bucket,
                    group,
                    path,
                    group_index,
                    bucket_index,
                    false, /* Don't wait and sync all data to storage media */
                )
                .await;
            }
        })
    }

    /// Immediate flush of all dirty buckets. Parallel across groups, sequential within a group.
    async fn flush_all(
        self: Arc<Self>,
        path: Option<Arc<PathBuf>>,
        sync_data: bool,
    ) -> Result<(), LocalMutableStoreError> {
        let Some(path) = path else {
            return Ok(());
        };
        let path = Arc::new(path.as_ref().clone());

        let mut tasks = JoinSet::new();
        let authoritative = self.authoritative;

        for (group_index, group) in self.group.iter().enumerate() {
            // Lock-free scan: skip entire group if nothing is dirty.
            let any_dirty = group
                .dirty
                .iter()
                .any(|flag| flag.load(atomic::Ordering::Relaxed));
            if !any_dirty {
                continue;
            }

            let group = group.clone();
            let path = path.clone();
            lore_base::lore_spawn!(tasks, async move {
                let mut first_err: Option<LocalMutableStoreError> = None;

                // Fan-out trigger: if any dirty bucket exceeds the threshold and we're below max level, redistribute entries before serializing.
                if let Err(err) =
                    maybe_fan_out_mutable_group(&group, path.as_ref(), group_index, authoritative)
                        .await
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
                    == MutableStoreVersion::LazyFanOut as u32;
                let needs_two_phase_commit = fan_out_aware && committed_level != active_buckets;

                if needs_two_phase_commit && first_err.is_none() {
                    // T10 two-phase commit. Every [0..active_buckets] bucket gets a .new file (skipping empties at index >= committed_level since no old file exists there to overwrite). After all .new files are durable, write level.pending as the commit point. Then rename .new -> final, write the level marker, delete level.pending. Recovery on the next store open rolls forward from any pending state.
                    if let Err(e) = lore_io::IoDriver::global()
                        .create_dir_all(&group_path)
                        .await
                        .map_err(|e| {
                            LocalMutableStoreError::internal_with_context(
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
                            let dirty = group.dirty[bucket_index].load(atomic::Ordering::Relaxed);
                            if !must_overwrite_old && !dirty {
                                continue;
                            }
                            // bucket() not try_bucket(): must_overwrite_old paths need a guard
                            // for serialize_to_new even on slots that were never touched.
                            let bucket = group.bucket(bucket_index).clone().read_owned().await;
                            // Re-check after lock acquire — concurrent paths may have just dirtied or undirtied this bucket.
                            if bucket.entry.is_empty() && !must_overwrite_old {
                                continue;
                            }
                            let res = MutableStoreBucket::serialize_to_new(
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
                        // No .new files were written for this group — skip the level.pending sentinel entirely. The sentinel exists to drive roll-forward recovery of a partially-completed transition; with no .new files there is no in-progress state to recover, so a direct marker write is sufficient. This restores ~256x throughput on the fresh-store-first-flush-with-sync_data case where most groups are empty (the common shape on `lore repository create`).
                        if first_err.is_none()
                            && let Err(err) = crate::local::fan_out::write_level_marker(
                                &group_path,
                                active_buckets,
                                sync_data,
                            )
                            .await
                            .map_err(|e| {
                                LocalMutableStoreError::internal_with_context(
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
                                LocalMutableStoreError::internal_with_context(
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
                                if let Err(err) = lore_io::IoDriver::global()
                                    .rename(&new_path, &final_path)
                                    .await
                                    && first_err.is_none()
                                {
                                    first_err = Some(
                                        LocalMutableStoreError::internal_with_context(
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
                            .await
                            .map_err(|e| {
                                LocalMutableStoreError::internal_with_context(
                                    e,
                                    "Failed to write level marker",
                                )
                            })
                        {
                            first_err = Some(err);
                        }

                        if first_err.is_none()
                            && let Err(err) =
                                crate::local::fan_out::delete_level_pending(&group_path)
                                    .await
                                    .map_err(|e| {
                                        LocalMutableStoreError::internal_with_context(
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
                        let res = MutableStoreBucket::serialize(
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
                    .map_err(|err| {
                        LocalMutableStoreError::internal_with_context(
                            err,
                            "mutable store flush task failed",
                        )
                    })
                    .flatten(),
            );
        }

        result
    }
}

#[async_trait]
impl crate::mutable_store::MutableStore for LocalMutableStore {
    // Assumes that payload has been validated to match the given hash prior to
    // calling this function to store the content payload - no hash validation done
    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        let key = Key::make_typed(key, key_type);
        let group_index = key.group_index();
        let group = &self.group[group_index];

        let (bucket_index, mut bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&key.hash(), n);
            let lock = group.bucket(idx).write().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) == n {
                break (idx, lock);
            }
            drop(lock);
        };

        if !bucket.deserialized && self.path.is_some() {
            Box::pin(bucket.deserialize(
                self.path.clone().unwrap().as_ref(),
                group_index,
                bucket_index,
                false,
                self.authoritative,
            ))
            .await
            .map_err(|e| {
                StoreError::internal_with_context(
                    e,
                    "Failed to deserialize mutable store bucket for store",
                )
            })?;
        }

        let (existing_value, match_made, insert_slot) = bucket.lookup(partition, key.hash());

        if match_made {
            // Previous entry found
            if existing_value == value {
                return Ok(());
            }
            let entry_index = bucket.sorted_index[insert_slot] as usize;
            bucket.entry[entry_index].value = value;
        } else {
            if value.is_zero() {
                return Ok(());
            }

            // inject new entry
            let count = bucket.entry.len();
            bucket.sorted_index.insert(insert_slot, count as u32);

            bucket.entry.push(MutableStoreEntry {
                key: key.hash(),
                partition,
                value,
            });
        }

        self.clone()
            .mark_dirty(&mut bucket, group_index, bucket_index);

        Ok(())
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let typed_key = Key::make_typed(key, key_type);
        let group_index = typed_key.group_index();
        let group = &self.group[group_index];

        // CAS-retry: re-read bucket_count after acquiring the bucket lock to detect a fan-out that landed between the index computation and the lock acquire. If layout changed, drop and retry.
        loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let bucket_index = crate::local::fan_out::bucket_index_for(&typed_key.hash(), n);
            let bucket_ref = group.bucket(bucket_index).clone();
            let mut bucket = bucket_ref.clone().read_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                drop(bucket);
                continue;
            }

            if !bucket.deserialized && self.path.is_some() {
                drop(bucket);
                let path = self.path.clone().unwrap();
                let authoritative = self.authoritative;
                let bucket_clone = bucket_ref.clone();
                let group_for_check = self.group[group_index].clone();
                let res = Box::pin(async move {
                    let mut bucket_write = bucket_clone.write_owned().await;
                    if group_for_check.bucket_count.load(atomic::Ordering::Relaxed) != n {
                        return Ok(false);
                    }
                    if !bucket_write.deserialized {
                        bucket_write
                            .deserialize(&path, group_index, bucket_index, false, authoritative)
                            .await
                            .map_err(|e| {
                                StoreError::internal_with_context(
                                    e,
                                    "Failed to deserialize mutable store bucket for get",
                                )
                            })?;
                    }
                    Ok::<_, StoreError>(true)
                })
                .await?;
                if !res {
                    continue;
                }
                bucket = bucket_ref.read_owned().await;
                if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                    drop(bucket);
                    continue;
                }
            }

            let (value, match_made, _) = bucket.lookup(partition, typed_key.hash());
            return if match_made && !value.is_zero() {
                Ok(value)
            } else {
                Err(StoreError::from(AddressNotFound::from(
                    Address::zero_context_hash(key),
                )))
            };
        }
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let key = Key::make_typed(key, key_type);
        let group_index = key.group_index();
        let group = &self.group[group_index];

        let (bucket_index, mut bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&key.hash(), n);
            let lock = group.bucket(idx).write().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) == n {
                break (idx, lock);
            }
            drop(lock);
        };

        if !bucket.deserialized && self.path.is_some() {
            Box::pin(bucket.deserialize(
                self.path.clone().unwrap().as_ref(),
                group_index,
                bucket_index,
                false,
                self.authoritative,
            ))
            .await
            .map_err(|e| {
                StoreError::internal_with_context(
                    e,
                    "Failed to deserialize mutable store bucket for compare-and-swap",
                )
            })?;
        }

        let (existing_value, match_made, insert_slot) = bucket.lookup(partition, key.hash());

        if match_made {
            // Previous entry found, check if we can CAS
            if existing_value != expected {
                // Value is not the expected, return stored value
                return Ok(existing_value);
            }
            // Value is the expected, swap to new value
            let entry_index = bucket.sorted_index[insert_slot] as usize;
            bucket.entry[entry_index].value = value;
        } else {
            // Previous entry not found, check if we can CAS
            if !expected.is_zero() {
                // Value is not the expected, return stored value
                return Ok(Hash::default());
            }

            // inject new entry
            let count = bucket.entry.len();
            bucket.sorted_index.insert(insert_slot, count as u32);

            bucket.entry.push(MutableStoreEntry {
                key: key.hash(),
                partition,
                value,
            });
        }

        self.clone()
            .mark_dirty(&mut bucket, group_index, bucket_index);

        // Value was created or updated, return previously stored value (the expected) to indicate this
        Ok(existing_value)
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        let (stream, sender) = KeyValueStream::new();

        if key_type == KeyType::Untyped {
            return Ok(stream);
        }

        for group_index in 0..self.group.len() {
            let path = self.path.clone();
            let authoritative = self.authoritative;
            let sender = sender.clone();
            let group = self.group[group_index].clone();
            let task = async move {
                let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
                for bucket_index in 0..active_buckets {
                    let bucket_ref = group.bucket(bucket_index).clone();
                    let mut bucket = bucket_ref.read().await;
                    let sender = sender.clone();

                    if !bucket.deserialized && path.is_some() {
                        drop(bucket);

                        let bucket_clone = bucket_ref.clone();
                        let path = path.clone();
                        Box::pin(async move {
                            let mut bucket_write = bucket_clone.write().await;
                            // TODO (raghav.narula) limit the number of deserialized buckets kept in memory on the client/cli
                            bucket_write
                                .deserialize(
                                    path.as_ref().unwrap(),
                                    group_index,
                                    bucket_index,
                                    false,
                                    authoritative,
                                )
                                .await
                                .map_err(|err| {
                                    StoreError::internal_with_context(
                                        err,
                                        "Failed to deserialize mutable store bucket",
                                    )
                                })
                        })
                        .await?;

                        bucket = bucket_ref.read().await;
                    }

                    fn handle_slot(
                        key_type: KeyType,
                        slot: usize,
                        partition: Partition,
                        bucket: &RwLockReadGuard<'_, MutableStoreBucket>,
                        sender: &UnboundedSender<(Hash, Hash)>,
                    ) -> Result<bool, StoreError> {
                        let index = bucket.sorted_index[slot];
                        let entry = bucket.entry[index as usize];
                        if Key::key_type_from_hash(&entry.key) == key_type as u8 {
                            if partition.is_zero() || entry.partition == partition {
                                sender.send((entry.key, entry.value)).map_err(|err| {
                                    StoreError::internal_with_context(
                                        err,
                                        "Failed to send mutable store entry while listing",
                                    )
                                })?;
                            }
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    }

                    // The bucket holds entries whose bucket byte (`data[1]`) falls in the
                    // contiguous range `[start_bucket_byte, start_bucket_byte + stride)`
                    // where `stride = 256 / active_buckets`. Within the bucket, sorted_index
                    // orders entries by full hash so by bucket byte primarily and key-type
                    // byte (`data[2]`) secondarily — equal-`key_type` entries are only
                    // contiguous within a single bucket-byte value. Carve sorted_index into
                    // one slice per bucket-byte value, then within each slice run the
                    // `lookup_any_with_key_type_in_range` binary search and walk neighbours
                    // bounded by the slice. At full fan-out (`stride == 1`) every entry
                    // already shares the bucket byte so the bucket itself is the only
                    // slice — skip the `upper_bound_bucket_byte` call and use
                    // `[0, bucket_len)` directly. Lookups per bucket: 256→1, 128→2, 64→4,
                    // 32→8, 1→256.
                    let bucket_len = bucket.sorted_index.len();
                    if bucket_len > 0 {
                        let stride = 256 / active_buckets;
                        let start_bucket_byte = bucket_index * stride;
                        let mut cursor = 0usize;
                        for byte_offset in 0..stride {
                            if cursor >= bucket_len {
                                break;
                            }
                            let hi = if stride == 1 {
                                bucket_len
                            } else {
                                let bucket_byte = (start_bucket_byte + byte_offset) as u8;
                                bucket.upper_bound_bucket_byte(cursor, bucket_len, bucket_byte)
                            };
                            if cursor < hi {
                                let (found, slot) =
                                    bucket.lookup_any_with_key_type_in_range(key_type, cursor, hi);
                                if found {
                                    handle_slot(key_type, slot, partition, &bucket, &sender)?;

                                    let mut loop_slot = slot;
                                    while loop_slot > cursor {
                                        loop_slot -= 1;
                                        if !handle_slot(
                                            key_type, loop_slot, partition, &bucket, &sender,
                                        )? {
                                            break;
                                        }
                                    }

                                    let mut loop_slot = slot + 1;
                                    while loop_slot < hi {
                                        if !handle_slot(
                                            key_type, loop_slot, partition, &bucket, &sender,
                                        )? {
                                            break;
                                        }
                                        loop_slot += 1;
                                    }
                                }
                            }
                            cursor = hi;
                        }
                    }
                }
                Ok::<(), StoreError>(())
            };

            lore_base::lore_spawn!(task);
        }

        Ok(stream)
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        if let Some(path) = self.path.as_ref() {
            self.clone()
                .flush_all(Some(path.clone()), sync_data)
                .await
                .map_err(|e| StoreError::internal_with_context(e, "Failed to flush store to disk"))
        } else {
            Ok(())
        }
    }
}

/// Inspect dirty buckets in `group` and, if any exceeds the per-store fan-out threshold and the
/// group is not yet at max level, atomically redistribute entries to the next ladder level.
///
/// Decision 6 protocol: take write locks on every bucket in `[0..M]` (where M is the target level),
/// redistribute, store the new `bucket_count`, then release. This is `Relaxed` on `bucket_count`
/// because the per-bucket `RwLock` releases publish the store via happens-before — readers and
/// writers using the CAS-retry pattern observe the change after their own re-load.
async fn maybe_fan_out_mutable_group(
    group: &Arc<MutableStoreGroup>,
    path: &Path,
    group_index: usize,
    authoritative: bool,
) -> Result<(), LocalMutableStoreError> {
    let n = group.bucket_count.load(atomic::Ordering::Relaxed);
    if n >= crate::local::fan_out::FAN_OUT_LEVEL_MAX {
        return Ok(());
    }
    // Scan dirty buckets briefly under read locks to find max entry count.
    let mut b_max = 0usize;
    for bucket_index in 0..n {
        if !group.dirty[bucket_index].load(atomic::Ordering::Relaxed) {
            continue;
        }
        let Some(bucket_ref) = group.try_bucket(bucket_index) else {
            continue;
        };
        let bucket = bucket_ref.read().await;
        b_max = b_max.max(bucket.entry.len());
    }
    if b_max <= group.fan_out_threshold {
        return Ok(());
    }
    let target = crate::local::fan_out::level_for(n, b_max, group.fan_out_threshold);
    if target <= n {
        return Ok(());
    }

    // Take write locks on ALL buckets [0..target] simultaneously. The [n..target] range is
    // uncontested since no caller computes an index ≥ n while bucket_count == n; their lock
    // releases publish fan-out's writes to subsequent readers/writers.
    let mut guards: Vec<tokio::sync::OwnedRwLockWriteGuard<MutableStoreBucket>> =
        Vec::with_capacity(target);
    for i in 0..target {
        guards.push(group.bucket(i).clone().write_owned().await);
    }

    // Force-deserialize any [0..n] bucket whose entries are still on disk only. Without this, on-disk-only buckets contribute zero entries to the redistribute and their data is lost when serialize overwrites their files with empty buckets at the new layout.
    for (bucket_index, guard) in guards.iter_mut().take(n).enumerate() {
        if !guard.deserialized {
            Box::pin(guard.deserialize(path, group_index, bucket_index, false, authoritative))
                .await?;
        }
    }

    // Drain entries from old buckets [0..n] into a temporary collection, then redistribute by
    // bucket_index_for(&key, target) into [0..target]. Each entry's destination bucket is computed
    // via the fan_out helper (high-bit selection).
    let mut entries_per_new_bucket: Vec<Vec<MutableStoreEntry>> =
        (0..target).map(|_| Vec::new()).collect();
    for guard in guards.iter_mut().take(n) {
        let old = std::mem::take(&mut guard.entry);
        for entry in old.iter() {
            let new_idx = crate::local::fan_out::bucket_index_for(&entry.key, target);
            entries_per_new_bucket[new_idx].push(*entry);
        }
        guard.sorted_index = lore_base::allocator::GrowVec::new();
    }

    // Repopulate target buckets.
    for (new_idx, entries) in entries_per_new_bucket.into_iter().enumerate() {
        let count = entries.len();
        let bucket = &mut guards[new_idx];
        bucket.entry = lore_base::allocator::GrowVec::new();
        bucket.sorted_index = lore_base::allocator::GrowVec::new();
        // Re-insert via the bucket's existing sort logic to keep sorted_index correct.
        for entry in entries {
            // Recompute insert slot using the bucket's own lookup path. partition+key.
            let (_existing, _match_made, insert_slot) = bucket.lookup(entry.partition, entry.key);
            let entry_index = bucket.entry.len();
            bucket.sorted_index.insert(insert_slot, entry_index as u32);
            bucket.entry.push(entry);
        }
        if count > 0 {
            group.dirty[new_idx].store(true, atomic::Ordering::Relaxed);
            // Mark deserialized so subsequent operations don't try to re-read from disk.
            bucket.deserialized = true;
        }
    }

    // Publish the new bucket_count BEFORE releasing locks; lock releases publish this Relaxed
    // store to subsequent acquirers via happens-before.
    group.bucket_count.store(target, atomic::Ordering::Relaxed);
    drop(guards);
    Ok(())
}

/// Create a mutable store for the given repository and .urc path
pub async fn create(
    path: Option<impl AsRef<Path>>,
    settings: MutableStoreSettings,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<Arc<LocalMutableStore>, StoreError> {
    let store = LocalMutableStore::new(path, settings, immutable_store)
        .await
        .map_err(|e| {
            StoreError::internal_with_context(e, "Failed to create data store for repository")
        })?;

    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bucket_file(path: &Path, version: u32) {
        let entry = MutableStoreEntry::default();
        let mut header = MutableStoreHeader::new_zeroed();
        header.version = version;
        header.count = 1;
        let mut bytes = Vec::with_capacity(
            size_of::<MutableStoreHeader>() + 4 + size_of::<MutableStoreEntry>(),
        );
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(entry.as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    /// A bucket larger than the head the composite open returns is loaded by scattering one
    /// vectored read into both `GrowVec`s. Per-index contents make a misplaced chunk visible as
    /// swapped entries rather than as a length mismatch.
    #[tokio::test]
    async fn deserialize_scatters_a_bucket_larger_than_the_head_read() {
        let dir = crate::test_util::TempDir::new("ms_scatter_");
        let path = dir.path().join("bucket");

        let head = crate::local::immutable_store::BUCKET_HEAD_READ;
        let per_entry = size_of::<u32>() + size_of::<MutableStoreEntry>();
        let count = (head / per_entry) + 64;
        assert!(
            size_of::<MutableStoreHeader>() + count * per_entry > head,
            "the bucket has to exceed the head read for this to test anything"
        );

        let mut header = MutableStoreHeader::new_zeroed();
        header.version = MutableStoreVersion::LazyFanOut as u32;
        header.count = count as u32;

        let mut bytes = Vec::with_capacity(size_of::<MutableStoreHeader>() + count * per_entry);
        bytes.extend_from_slice(header.as_bytes());
        for index in 0..count {
            bytes.extend_from_slice(&(index as u32).to_le_bytes());
        }
        for index in 0..count {
            let entry = MutableStoreEntry {
                key: Hash::from([index as u8; 32]),
                ..Default::default()
            };
            bytes.extend_from_slice(entry.as_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let (sorted_index, entry, version) = MutableStoreBucket::deserialize_files(path, false)
            .await
            .unwrap();

        assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
        assert_eq!(sorted_index.len(), count);
        assert_eq!(entry.len(), count);
        for index in 0..count {
            assert_eq!(sorted_index[index], index as u32, "sorted index at {index}");
            assert_eq!(
                entry[index].key,
                Hash::from([index as u8; 32]),
                "key at {index}"
            );
        }
    }

    #[tokio::test]
    async fn deserialize_accepts_typed_items_v2() {
        let dir = crate::test_util::TempDir::new("ms_v2_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, MutableStoreVersion::TypedItems as u32);
        let result = MutableStoreBucket::deserialize_files(path, false).await;
        assert!(result.is_ok(), "v2 (TypedItems) bucket should deserialize");
    }

    #[tokio::test]
    async fn deserialize_accepts_lazy_fan_out_v3() {
        let dir = crate::test_util::TempDir::new("ms_v3_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, MutableStoreVersion::LazyFanOut as u32);
        let result = MutableStoreBucket::deserialize_files(path, false).await;
        assert!(result.is_ok(), "v3 (LazyFanOut) bucket should deserialize");
    }

    #[tokio::test]
    async fn deserialize_rejects_unknown_future_version() {
        let dir = crate::test_util::TempDir::new("ms_v100_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, 100);
        let result = MutableStoreBucket::deserialize_files(path, false).await;
        assert!(result.is_err(), "v100 bucket should be rejected as too new");
    }

    /// A torn write can leave a bucket file at its correct byte length but entirely
    /// zero-filled, so the header reads count=0 while the size implies a nonzero count. This
    /// recovers to an empty bucket rather than hard-erroring.
    #[tokio::test]
    async fn deserialize_recovers_zero_filled_bucket() {
        let dir = crate::test_util::TempDir::new("ms_zerofill_");
        let path = dir.path().join("bucket");
        // Correct length for a 1-entry bucket, but all zeros.
        let len =
            size_of::<MutableStoreHeader>() + size_of::<u32>() + size_of::<MutableStoreEntry>();
        std::fs::write(&path, vec![0u8; len]).unwrap();

        let (sorted_index, entry, version) = MutableStoreBucket::deserialize_files(path, false)
            .await
            .expect("zero-filled bucket should recover to empty");
        assert_eq!(sorted_index.len(), 0);
        assert_eq!(entry.len(), 0);
        assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
    }

    #[tokio::test]
    async fn deserialize_authoritative_errors_and_preserves_corrupt_bucket() {
        let dir = crate::test_util::TempDir::new("ms_auth_corrupt_");
        let path = dir.path().join("bucket");
        let len =
            size_of::<MutableStoreHeader>() + size_of::<u32>() + size_of::<MutableStoreEntry>();
        std::fs::write(&path, vec![0u8; len]).unwrap();

        let result = MutableStoreBucket::deserialize_files(path.clone(), true).await;
        assert!(
            result.is_err(),
            "authoritative store must not reset a corrupt bucket"
        );
        assert!(
            path.exists(),
            "authoritative store must preserve the corrupt bucket file"
        );
    }

    #[test]
    fn lazy_fan_out_version_is_three() {
        assert_eq!(MutableStoreVersion::LazyFanOut as u32, 3);
    }

    #[tokio::test]
    async fn latest_version_constant_in_deserialize_path_matches_lazy_fan_out() {
        let dir = crate::test_util::TempDir::new("ms_latest_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, MutableStoreVersion::LazyFanOut as u32);
        let (_, _, version) = MutableStoreBucket::deserialize_files(path, false)
            .await
            .unwrap();
        assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
    }

    #[test]
    fn mutable_store_settings_default_is_client_friendly() {
        let s = MutableStoreSettings::default();
        assert_eq!(s.flush_delay_seconds, DEFAULT_FLUSH_DELAY_SECONDS);
        assert_eq!(s.initial_fan_out_level, 1);
        assert_eq!(
            s.fan_out_threshold,
            crate::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT
        );
    }

    /// End-to-end: after a bucket file is zero-filled, the store still opens and the bucket is
    /// usable for a store/load round-trip.
    #[tokio::test]
    async fn store_recovers_from_zero_filled_bucket_and_remains_usable() {
        use crate::mutable_store::MutableStore;

        let dir = crate::test_util::TempDir::new("ms_e2e_recover_");
        let store_path = dir.path().to_path_buf();
        let partition = Partition::default();
        let mut key = Hash::default();
        key.data_mut()[0] = 0x10;
        key.data_mut()[1] = 0xAB;
        let value = Hash::from_u64(42);

        {
            let store: Arc<dyn MutableStore> = Arc::new(
                LocalMutableStore::new(
                    Some(&store_path),
                    MutableStoreSettings {
                        initial_fan_out_level: 1,
                        ..Default::default()
                    },
                    make_in_memory_immutable().await,
                )
                .await
                .unwrap(),
            );
            store
                .clone()
                .store(partition, key, value, KeyType::BranchMetadata)
                .await
                .unwrap();
            store.clone().flush(true).await.unwrap();
        }

        // initial_fan_out_level=1 → bucket index is always 0; group is (typed) key[0].
        // `LocalMutableStore::new` roots the store under a `mutable/` subdirectory.
        let group_index = key.data()[0] as usize;
        let bucket_path = format_bucket_path(&store_path.join("mutable"), group_index, 0);
        assert!(
            bucket_path.exists(),
            "bucket file should exist after flush at {bucket_path:?}"
        );

        // Torn write: correct byte length, entirely zero-filled.
        let len = std::fs::metadata(&bucket_path).unwrap().len() as usize;
        std::fs::write(&bucket_path, vec![0u8; len]).unwrap();

        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );

        store
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .unwrap();
        let reloaded = store
            .clone()
            .load(partition, key, KeyType::BranchMetadata)
            .await
            .unwrap();
        assert_eq!(reloaded, value, "bucket must be usable after recovery");
    }

    async fn make_in_memory_immutable() -> Arc<dyn ImmutableStore> {
        crate::local::immutable_store::create(
            None::<&str>,
            crate::local::immutable_store::ImmutableStoreCreateOptions::none(),
            false,
            crate::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create in-memory immutable store")
    }

    #[tokio::test]
    async fn store_initializes_group_bucket_count_from_settings_level_1() {
        use std::sync::atomic::Ordering;
        let store = LocalMutableStore::new(
            None::<&Path>,
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            make_in_memory_immutable().await,
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
        let store = LocalMutableStore::new(
            None::<&Path>,
            MutableStoreSettings {
                initial_fan_out_level: crate::local::fan_out::FAN_OUT_LEVEL_MAX,
                ..Default::default()
            },
            make_in_memory_immutable().await,
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

    #[tokio::test]
    async fn level_1_store_and_load_round_trip() {
        use crate::mutable_store::MutableStore;
        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                None::<&Path>,
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        let partition = Partition::default();
        let mut key = Hash::default();
        // Set bytes that, at level 256, would route to bucket 0xAB; at level 1 must still route to bucket 0.
        key.data_mut()[0] = 0x10;
        key.data_mut()[1] = 0xAB;
        let value = Hash::from_u64(42);
        store
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .unwrap();
        let loaded = store
            .clone()
            .load(partition, key, KeyType::BranchMetadata)
            .await
            .unwrap();
        assert_eq!(loaded, value);
    }

    /// At fan-out levels < 256 a single bucket holds entries spanning several bucket-byte
    /// (`data[1]`) values, so within the bucket the full-hash sort orders entries primarily
    /// by bucket byte and only secondarily by `data[2]` (the key-type byte). A binary
    /// search that compares only `data[2]` can land on an entry whose bucket byte differs
    /// from the target's and erroneously conclude no match exists, missing entries that
    /// are actually present. The fix carves the bucket's `sorted_index` into one slice per
    /// bucket-byte value before running the per-slice key-type search. This regression
    /// test inserts one `Instance` and one `BranchMetadata` entry into the same bucket
    /// at each level in the ladder and verifies `list(Instance)` returns the `Instance`
    /// entry; phase two adds two more `Instance` entries plus a mix of filler entries
    /// across the bucket's bucket-byte range and verifies all three `Instance` entries
    /// are enumerated.
    #[tokio::test]
    async fn list_finds_typed_entries_at_each_fan_out_level() {
        use futures::StreamExt;

        use crate::mutable_store::MutableStore;

        for &level in &[1usize, 32, 64, 128, 256] {
            let store: Arc<dyn MutableStore> = Arc::new(
                LocalMutableStore::new(
                    None::<&Path>,
                    MutableStoreSettings {
                        initial_fan_out_level: level,
                        ..Default::default()
                    },
                    make_in_memory_immutable().await,
                )
                .await
                .unwrap(),
            );

            let partition = Partition::default();
            let stride = 256 / level;

            // Phase 1: insert one Instance and one BranchMetadata in the same bucket and
            // verify list(Instance) finds the Instance. The simple two-entry shape is the
            // original failing case from the test_background_prune_during_clone smoke
            // flake.
            let d1_inst1 = 0u8;
            let d1_meta = if stride >= 2 { 1u8 } else { 0u8 };

            let mut k_inst1 = Hash::default();
            k_inst1.data_mut()[0] = 0x42;
            k_inst1.data_mut()[1] = d1_inst1;

            let mut k_meta = Hash::default();
            k_meta.data_mut()[0] = 0x42;
            k_meta.data_mut()[1] = d1_meta;
            if d1_inst1 == d1_meta {
                k_meta.data_mut()[3] = 1;
            }

            let v_inst1 = Hash::from_u64(1);
            let v_meta = Hash::from_u64(2);
            store
                .clone()
                .store(partition, k_inst1, v_inst1, KeyType::Instance)
                .await
                .unwrap();
            store
                .clone()
                .store(partition, k_meta, v_meta, KeyType::BranchMetadata)
                .await
                .unwrap();

            let mut stream = store
                .clone()
                .list(partition, KeyType::Instance)
                .await
                .unwrap();
            let mut found_phase1: Vec<(Hash, Hash)> = Vec::new();
            while let Some(item) = stream.next().await {
                found_phase1.push(item);
            }
            assert_eq!(
                found_phase1.len(),
                1,
                "level {level} phase 1: list(Instance) returned {} entries, expected 1",
                found_phase1.len()
            );
            assert_eq!(
                found_phase1[0].1, v_inst1,
                "level {level} phase 1: wrong value returned"
            );

            // Phase 2: insert two more Instance entries plus a mix of non-Instance entries
            // — all into the same bucket. At fan-out levels < 256 the entries take distinct
            // bucket-byte values within the single bucket's range, exercising the per-slice
            // walk over scattered Instance entries. At level 256 only one bucket-byte value
            // routes to a given bucket, so the two extras share `data[1]` with the first
            // and are differentiated via `data[5]`; this exercises the within-bucket
            // `stride == 1` fast path with multiple Instance entries packed together.
            let (d1_inst2, d1_inst3) = if stride >= 2 {
                ((stride / 2) as u8, (stride - 1) as u8)
            } else {
                (0u8, 0u8)
            };

            let mut k_inst2 = Hash::default();
            k_inst2.data_mut()[0] = 0x42;
            k_inst2.data_mut()[1] = d1_inst2;
            k_inst2.data_mut()[5] = 1;

            let mut k_inst3 = Hash::default();
            k_inst3.data_mut()[0] = 0x42;
            k_inst3.data_mut()[1] = d1_inst3;
            k_inst3.data_mut()[5] = 2;

            let v_inst2 = Hash::from_u64(11);
            let v_inst3 = Hash::from_u64(12);
            store
                .clone()
                .store(partition, k_inst2, v_inst2, KeyType::Instance)
                .await
                .unwrap();
            store
                .clone()
                .store(partition, k_inst3, v_inst3, KeyType::Instance)
                .await
                .unwrap();

            let other_kts = [
                KeyType::BranchMetadata,
                KeyType::BranchId,
                KeyType::BranchLatestPointer,
                KeyType::RepositoryMetadata,
                KeyType::RepositoryId,
            ];
            let filler_d1_max = stride.min(8);
            let mut counter: u64 = 100;
            for d1_idx in 0..filler_d1_max {
                let d1 = d1_idx as u8;
                for &kt in &other_kts {
                    let mut k = Hash::default();
                    k.data_mut()[0] = 0x42;
                    k.data_mut()[1] = d1;
                    k.data_mut()[6] = (counter & 0xff) as u8;
                    k.data_mut()[7] = ((counter >> 8) & 0xff) as u8;
                    store
                        .clone()
                        .store(partition, k, Hash::from_u64(counter), kt)
                        .await
                        .unwrap();
                    counter += 1;
                }
            }

            // Phase 3: list(Instance) must return all three Instance entries despite the
            // filler entries scattered through the bucket.
            let mut stream = store
                .clone()
                .list(partition, KeyType::Instance)
                .await
                .unwrap();
            let mut found_phase3: Vec<Hash> = Vec::new();
            while let Some((_k, v)) = stream.next().await {
                found_phase3.push(v);
            }
            found_phase3.sort();
            let mut expected = vec![v_inst1, v_inst2, v_inst3];
            expected.sort();
            assert_eq!(
                found_phase3, expected,
                "level {level} phase 3: expected three Instance entries, got {found_phase3:?}"
            );

            // Phase 4 (fan-out levels > 1 only): populate two additional buckets — bucket 5
            // and bucket 10 — each with one Instance entry plus filler entries spanning the
            // full bucket-byte sub-range of that bucket. This exercises cross-bucket
            // enumeration AND the per-slice walk inside each non-zero bucket: at fan-out
            // levels < 256 the new buckets each hold entries with `stride` distinct
            // bucket-byte values, so finding the Instance still requires walking past
            // non-matching slices. Skipped at level 1 because only bucket 0 exists.
            if level > 1 {
                let extra_buckets = [5usize, 10usize];
                let mut extra_inst_values: Vec<Hash> = Vec::new();
                for (next_inst_value, &bucket_idx) in (13u64..).zip(extra_buckets.iter()) {
                    let d1_lo = bucket_idx * stride;
                    let d1_hi = d1_lo + stride;

                    let mut k_inst_extra = Hash::default();
                    k_inst_extra.data_mut()[0] = 0x42;
                    k_inst_extra.data_mut()[1] = d1_lo as u8;
                    let v_inst_extra = Hash::from_u64(next_inst_value);
                    store
                        .clone()
                        .store(partition, k_inst_extra, v_inst_extra, KeyType::Instance)
                        .await
                        .unwrap();
                    extra_inst_values.push(v_inst_extra);

                    for d1_value in d1_lo..d1_hi {
                        for &kt in &other_kts {
                            let mut k = Hash::default();
                            k.data_mut()[0] = 0x42;
                            k.data_mut()[1] = d1_value as u8;
                            k.data_mut()[6] = (counter & 0xff) as u8;
                            k.data_mut()[7] = ((counter >> 8) & 0xff) as u8;
                            store
                                .clone()
                                .store(partition, k, Hash::from_u64(counter), kt)
                                .await
                                .unwrap();
                            counter += 1;
                        }
                    }
                }

                let mut stream = store
                    .clone()
                    .list(partition, KeyType::Instance)
                    .await
                    .unwrap();
                let mut found_phase4: Vec<Hash> = Vec::new();
                while let Some((_k, v)) = stream.next().await {
                    found_phase4.push(v);
                }
                found_phase4.sort();
                let mut expected = vec![v_inst1, v_inst2, v_inst3];
                expected.extend(extra_inst_values);
                expected.sort();
                assert_eq!(
                    found_phase4,
                    expected,
                    "level {level} phase 4: expected {} Instance entries across multiple \
                     buckets, got {found_phase4:?}",
                    expected.len()
                );
            }
        }
    }
}
