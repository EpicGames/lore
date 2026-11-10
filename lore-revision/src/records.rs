// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The modified times an instance has recorded for the files of its working tree, kept in the
//! instance's own dot directory.
//!
//! A record states that a path held the current revision's content at a modification time, so a
//! scan can skip hashing a file whose time still matches. Only the instance reads its records,
//! so they belong to its repository lock. A record's key is taken from the path and not the
//! instance, so a copy of the dot directory carries records that describe the copy's files.
//!
//! The records are split into shards by the first byte of the key. A shard is an array of 24-byte
//! records sorted by key, and its file is a header followed by the same bytes. A record whose time
//! is 0 is removed, and a write-back leaves it out.
//!
//! Records are advisory: a missing record costs a hash, never a wrong answer. A shard file that
//! cannot be read, or whose structure or format version this client does not recognize, is
//! treated as absent, and the next write-back replaces it.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(feature = "test-util")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::allocator::GrowVec;
use lore_base::lore_drain_tasks;
use lore_base::lore_spawn;
use lore_base::types::Hash;
use lore_storage::local::immutable_store::SerializeFailureGuard;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::RwLock;
use tokio::sync::RwLockWriteGuard;
use tokio::task::JoinSet;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::little_endian::U64;

use crate::lore_warn;
use crate::repository::RepositoryWriteToken;

/// Directory in the instance's dot directory that holds the shard files.
pub const RECORDS_DIR: &str = "records";

/// File in [`RECORDS_DIR`] that marks an instance as one that has never kept records in the
/// mutable store. See [`mark_new_instance`].
#[lore_macro::test_pub]
pub(crate) const NO_STORE_RECORDS: &str = "no-store";

const SHARD_COUNT: usize = 256;
#[lore_macro::test_pub]
const MAGIC: &[u8; 4] = b"LOMT";
#[lore_macro::test_pub]
const FORMAT_VERSION: u32 = 1;
const HEADER_SIZE: usize = MAGIC.len() + size_of::<u32>();
const KEY_SIZE: usize = 16;

/// Bytes a shard load reads as it opens the file. A tree of a million files has shards of about
/// 100 KB, which fit, and a larger shard takes one more read.
const SHARD_HEAD_READ: usize = 256 * 1024;

/// The most records a shard file can hold and be read: 100 MB of records, about a billion files
/// across the shards. A larger file is treated as unreadable rather than allocated.
#[lore_macro::test_pub]
const SHARD_RECORDS_MAX: usize = 1 << 22;

/// Records per allocation of a shard.
const RECORD_CHUNK: usize = 64;

/// The fewest times a shard collects for keys it does not hold before merging them in.
const MERGE_MIN: usize = 256;

/// The key a record is filed under: 16 bytes of a hash of the repository's salt and the
/// lowercased path.
pub type RecordKey = [u8; KEY_SIZE];

/// A time recorded for a key, as a shard holds it in memory and in its file. The time is stored
/// little-endian, so a shard file reads the same on every host.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable)]
pub struct Record {
    pub key: RecordKey,
    mtime: U64,
}

impl Record {
    /// A record of `mtime` for `key`; 0 marks it removed.
    pub fn new(key: RecordKey, mtime: u64) -> Self {
        Self {
            key,
            mtime: U64::new(mtime),
        }
    }

    /// The modification time, or 0 for a removed record.
    pub fn mtime(&self) -> u64 {
        self.mtime.get()
    }
}

/// What [`InstanceRecords::lookup`] found for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The time recorded for the key.
    Recorded(u64),
    /// Nothing is recorded. The generation lets [`InstanceRecords::adopt`] tell whether the
    /// shard changed since.
    Missing(u64),
}

/// Records for [`InstanceRecords::set_all`] to set in one pass, grouped by shard.
pub struct RecordBatch {
    shards: Vec<Vec<Record>>,
}

impl RecordBatch {
    /// A batch sized for about `len` records, which keys spread evenly across the shards.
    pub fn with_capacity(len: usize) -> Self {
        let per_shard = len / SHARD_COUNT * 11 / 10;
        Self {
            shards: (0..SHARD_COUNT)
                .map(|_| Vec::with_capacity(per_shard))
                .collect(),
        }
    }

    pub fn push(&mut self, record: Record) {
        self.shards[record.key[0] as usize].push(record);
    }
}

#[derive(Default)]
struct Shard {
    /// Sorted by key, with no key twice: the shard file's body, byte for byte.
    records: GrowVec<Record, RECORD_CHUNK>,
    /// Times set for keys `records` does not hold, to be merged into it in one pass. A later
    /// entry for a key replaces an earlier one. Lookups do not see them.
    pending: Vec<Record>,
    /// Times read from the mutable store for keys `records` does not hold, merged with
    /// `pending`, whose entry for a key wins. Lookups do not see them.
    adopted: Vec<Record>,
    /// The records in `records` whose time is 0.
    removed: usize,
    deserialized: bool,
    /// Counts the times set and removed since the shard was loaded.
    generation: u64,
}

impl Shard {
    fn search(&self, key: &RecordKey) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.records.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.records[middle].key.cmp(key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(low)
    }

    fn lookup(&self, key: &RecordKey) -> Lookup {
        match self.search(key) {
            Ok(index) if self.records[index].mtime() != 0 => {
                Lookup::Recorded(self.records[index].mtime())
            }
            _ => Lookup::Missing(self.generation),
        }
    }

    /// Records `mtime` for `key`, in place where `records` holds the key, and reports whether
    /// anything changed.
    fn set(&mut self, key: RecordKey, mtime: u64) -> bool {
        match self.search(&key) {
            Ok(index) => {
                let record = &mut self.records[index];
                if record.mtime() == mtime {
                    return false;
                }
                match (record.mtime(), mtime) {
                    (0, _) => self.removed -= 1,
                    (_, 0) => self.removed += 1,
                    _ => {}
                }
                record.mtime = U64::new(mtime);
            }
            Err(_) => self.pending.push(Record::new(key, mtime)),
        }
        true
    }

    /// Merges the collected times once there are enough of them.
    fn merge_if_due(&mut self) {
        if self.pending.len() + self.adopted.len() > MERGE_MIN.max(self.records.len() / 8) {
            self.merge();
        }
    }

    /// Merges the pending and adopted times into `records` and leaves out the removed records,
    /// in one pass.
    fn merge(&mut self) {
        if self.pending.is_empty() && self.adopted.is_empty() && self.removed == 0 {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending);
        // Reversed, then sorted stably, so the latest time set for a key comes first.
        pending.reverse();
        pending.sort_by_key(|record| record.key);
        pending.dedup_by(|later, kept| later.key == kept.key);
        let mut adopted = std::mem::take(&mut self.adopted);
        adopted.sort_unstable_by_key(|record| record.key);
        adopted.dedup_by(|later, kept| later.key == kept.key);

        let mut merged = GrowVec::new();
        let mut records = self.records.iter().copied().peekable();
        let mut pending = pending.into_iter().peekable();
        let mut adopted = adopted.into_iter().peekable();
        loop {
            let next = [records.peek(), pending.peek(), adopted.peek()]
                .into_iter()
                .flatten()
                .map(|record| record.key)
                .min();
            let Some(key) = next else {
                break;
            };
            // A pending time wins over an adopted one. `records` shares no key with either.
            let mut chosen = adopted.next_if(|record| record.key == key);
            if let Some(record) = records.next_if(|record| record.key == key) {
                chosen = Some(record);
            }
            if let Some(record) = pending.next_if(|record| record.key == key) {
                chosen = Some(record);
            }
            if let Some(record) = chosen
                && record.mtime() != 0
            {
                merged.push(record);
            }
        }
        self.records = merged;
        self.removed = 0;
    }
}

struct ShardSlot {
    /// Set once the shard's file has been read, so a lookup can tell without taking the lock.
    loaded: AtomicBool,
    /// Set by a change the shard's file does not hold yet.
    dirty: AtomicBool,
    shard: Arc<RwLock<Shard>>,
    /// Held by a write of the shard's file until the file has landed, so that no two writes
    /// share its temporary file. A write runs on after a write-back is dropped.
    writing: Arc<tokio::sync::Mutex<()>>,
}

/// One instance's records, loaded a shard at a time as keys reach them and written back by
/// [`write_back`](Self::write_back).
pub struct InstanceRecords {
    directory: PathBuf,
    /// Whether the instance is marked as one that has never kept records in the mutable store.
    no_store_records: bool,
    shards: Box<[ShardSlot]>,
    #[cfg(feature = "test-util")]
    loads: AtomicUsize,
}

impl InstanceRecords {
    /// The records of the instance whose dot directory is `dot_path`. Reads whether the instance
    /// is marked by [`mark_new_instance`], and no shard until a key reaches it.
    pub async fn open(dot_path: &Path) -> Arc<Self> {
        let directory = dot_path.join(RECORDS_DIR);
        let no_store_records = lore_io::IoDriver::global()
            .metadata(directory.join(NO_STORE_RECORDS))
            .await
            .is_ok();
        Arc::new(Self {
            directory,
            no_store_records,
            shards: (0..SHARD_COUNT)
                .map(|_| ShardSlot {
                    loaded: AtomicBool::new(false),
                    dirty: AtomicBool::new(false),
                    shard: Arc::default(),
                    writing: Arc::default(),
                })
                .collect(),
            #[cfg(feature = "test-util")]
            loads: AtomicUsize::new(0),
        })
    }

    /// The record key taken from a hash of the path.
    pub fn key(hash: &Hash) -> RecordKey {
        std::array::from_fn(|index| hash.data()[index])
    }

    /// Whether the mutable store can hold records of this instance that an older client wrote.
    /// Only then does a lookup that misses read the store, and a changed record zero the store's.
    pub fn has_store_records(&self) -> bool {
        !self.no_store_records
    }

    /// What is recorded for `key`. A time set for a new key, or adopted, is seen once the shard
    /// has merged it.
    pub async fn lookup(&self, key: &RecordKey) -> Lookup {
        let slot = &self.shards[key[0] as usize];
        if slot.loaded.load(Ordering::Acquire)
            && let Ok(shard) = slot.shard.try_read()
        {
            return shard.lookup(key);
        }
        // Boxed: taken to load the shard, once, and otherwise only while a change holds it.
        Box::pin(self.lookup_waiting(*key)).await
    }

    async fn lookup_waiting(&self, key: RecordKey) -> Lookup {
        let slot = &self.shards[key[0] as usize];
        if slot.loaded.load(Ordering::Acquire) {
            return slot.shard.read().await.lookup(&key);
        }
        self.write_shard(key[0]).await.lookup(&key)
    }

    /// Records `mtime` for `key`.
    pub fn set(&self, key: RecordKey, mtime: u64) -> impl Future<Output = ()> + '_ {
        self.change(key[0], move |shard| {
            if shard.set(key, mtime) {
                shard.generation += 1;
                shard.merge_if_due();
                self.mark_dirty(key[0]);
            }
        })
    }

    /// Records every time in `batch`, and merges each shard it reaches. The shards are loaded
    /// and merged side by side, one task each.
    ///
    /// # Errors
    ///
    /// A task that did not complete. The shards of the other tasks are still set.
    pub async fn set_all(self: &Arc<Self>, batch: RecordBatch) -> std::io::Result<()> {
        let mut tasks = JoinSet::new();
        for (index, records) in batch.shards.into_iter().enumerate() {
            if !records.is_empty() {
                lore_spawn!(tasks, Arc::clone(self).set_shard(index as u8, records));
            }
        }
        lore_drain_tasks!(
            tasks,
            std::io::Error::other("setting modified time records did not complete")
        )
    }

    /// Records every time in `records`, all of shard `index`, and merges the shard.
    async fn set_shard(self: Arc<Self>, index: u8, records: Vec<Record>) -> std::io::Result<()> {
        let mut shard = self.write_shard(index).await;
        if shard.records.is_empty() && shard.pending.is_empty() {
            shard.pending = records;
        } else {
            for record in records {
                shard.set(record.key, record.mtime());
            }
        }
        shard.generation += 1;
        shard.merge();
        self.mark_dirty(index);
        Ok(())
    }

    /// Records `mtime` for `key`, read from the mutable store after a [`lookup`](Self::lookup)
    /// returned `generation`, unless a time has been set or removed in the shard since. Such a
    /// change can be newer than the time read, which then no longer describes the path.
    pub fn adopt(
        &self,
        key: RecordKey,
        mtime: u64,
        generation: u64,
    ) -> impl Future<Output = ()> + '_ {
        self.change(key[0], move |shard| {
            if shard.generation == generation && shard.search(&key).is_err() {
                shard.adopted.push(Record::new(key, mtime));
                shard.merge_if_due();
                self.mark_dirty(key[0]);
            }
        })
    }

    /// Removes the record for `key`.
    pub fn remove(&self, key: &RecordKey) -> impl Future<Output = ()> + '_ {
        let key = *key;
        self.change(key[0], move |shard| {
            // The generation moves either way, so no time read for the key before this is
            // adopted after it.
            shard.generation += 1;
            if shard.search(&key).is_err() {
                if shard.pending.is_empty() && shard.adopted.is_empty() {
                    return;
                }
                // The key may have a time waiting to be merged, which this replaces.
                shard.pending.push(Record::new(key, 0));
            } else if !shard.set(key, 0) {
                return;
            }
            self.mark_dirty(key[0]);
        })
    }

    /// Writes every shard changed since it was loaded or last written, all at once. `hold` is
    /// kept until each write has landed, whether or not this call is still awaited.
    ///
    /// # Errors
    ///
    /// The first error writing a shard. The other shards are still written, and a shard that
    /// failed stays changed, so a later write-back tries it again.
    pub async fn write_back<H: Clone + Send + 'static>(
        self: &Arc<Self>,
        _token: &RepositoryWriteToken,
        hold: H,
    ) -> std::io::Result<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        lore_io::IoDriver::global()
            .create_dir_all(&self.directory)
            .await?;
        let mut tasks = JoinSet::new();
        for index in 0..SHARD_COUNT {
            if self.shards[index].dirty.load(Ordering::Acquire) {
                lore_spawn!(
                    tasks,
                    Arc::clone(self).write_shard_file(index as u8, hold.clone())
                );
            }
        }
        lore_drain_tasks!(
            tasks,
            std::io::Error::other("writing modified time records did not complete")
        )
    }

    /// Whether any shard has a change its file does not hold yet.
    pub fn is_dirty(&self) -> bool {
        self.shards
            .iter()
            .any(|slot| slot.dirty.load(Ordering::Acquire))
    }

    /// How many shard files have been read. Test builds only.
    #[cfg(feature = "test-util")]
    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    fn mark_dirty(&self, index: u8) {
        self.shards[index as usize]
            .dirty
            .store(true, Ordering::Release);
    }

    fn shard_path(&self, index: u8) -> PathBuf {
        self.directory.join(format!("{index:02x}"))
    }

    /// Applies `change` to shard `index` under its write lock. Where the shard is loaded and no
    /// one holds its lock, the lock is taken without waiting, and the change adds no await.
    async fn change<R>(&self, index: u8, change: impl FnOnce(&mut Shard) -> R) -> R {
        let slot = &self.shards[index as usize];
        if slot.loaded.load(Ordering::Acquire)
            && let Ok(mut shard) = slot.shard.try_write()
        {
            return change(&mut shard);
        }
        // Boxed: taken to load the shard, once, and otherwise only while another task holds it.
        let mut shard = Box::pin(self.write_shard(index)).await;
        change(&mut shard)
    }

    /// The shard `index` under its write lock, read from its file first if it is not yet.
    async fn write_shard(&self, index: u8) -> RwLockWriteGuard<'_, Shard> {
        let mut shard = self.shards[index as usize].shard.write().await;
        if !shard.deserialized {
            // Boxed: runs once per shard, and inline it would be part of every change's future.
            Box::pin(self.load(index, &mut shard)).await;
        }
        shard
    }

    async fn load(&self, index: u8, shard: &mut Shard) {
        #[cfg(feature = "test-util")]
        self.loads.fetch_add(1, Ordering::Relaxed);
        match read_shard(&self.shard_path(index), index).await {
            Ok(records) => {
                shard.removed = records.iter().filter(|record| record.mtime() == 0).count();
                shard.records = records;
            }
            Err(ShardError::Absent) => {}
            Err(ShardError::Unreadable) => {
                lore_warn!(
                    "Ignoring unreadable modified time records {}",
                    self.shard_path(index).display()
                );
                // Replaced at the next write-back, so no record it still holds outlives a change
                // this client made to the path.
                self.mark_dirty(index);
            }
        }
        shard.deserialized = true;
        self.shards[index as usize]
            .loaded
            .store(true, Ordering::Release);
    }

    /// Writes shard `index` beside its file and renames it over, so a reader or a crash never
    /// sees a partial shard. The shard stays read-locked until the write has its bytes, and the
    /// shard's `writing` lock and `hold` are kept until the file has landed.
    #[lore_macro::test_pub]
    async fn write_shard_file<H: Send + 'static>(
        self: Arc<Self>,
        index: u8,
        hold: H,
    ) -> std::io::Result<()> {
        let slot = &self.shards[index as usize];
        let writing = Arc::clone(&slot.writing).lock_owned().await;
        if !slot.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let mut failure = SerializeFailureGuard::new(&slot.dirty);
        let mut shard = Arc::clone(&slot.shard).write_owned().await;
        shard.merge();
        let segments = ShardWriteSegments {
            header: Box::new(header()),
            shard: shard.downgrade(),
        };
        lore_io::IoDriver::global()
            .write_file_segments_atomic(
                self.directory.join(format!("{index:02x}.tmp")),
                self.shard_path(index),
                &lore_io::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true),
                segments,
                false,
                (hold, writing),
            )
            .await?;
        failure.success();
        Ok(())
    }
}

/// Marks the instance whose dot directory is `dot_path` as one that has never kept records in
/// the mutable store, for an instance being created, and clears any records the directory
/// holds. A marked instance's lookups never read the store, and its changes zero nothing there.
/// A copy of the dot directory carries the mark.
///
/// # Errors
///
/// The error removing the records or writing the mark.
pub async fn mark_new_instance(
    dot_path: &Path,
    _token: &RepositoryWriteToken,
) -> std::io::Result<()> {
    match lore_io::IoDriver::global()
        .remove_dir_all(dot_path.join(RECORDS_DIR))
        .await
    {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    mark_holding_no_store_records(dot_path).await
}

/// Marks the instance whose Lore directory is `dot_path` as one that has never kept records in
/// the mutable store, as [`mark_new_instance`] does, without clearing the records the directory
/// holds.
///
/// # Errors
///
/// The error writing the mark.
pub(crate) async fn mark_holding_no_store_records(dot_path: &Path) -> std::io::Result<()> {
    let directory = dot_path.join(RECORDS_DIR);
    lore_io::IoDriver::global()
        .create_dir_all(&directory)
        .await?;
    lore_io::IoDriver::global()
        .write_file_bytes(directory.join(NO_STORE_RECORDS), bytes::Bytes::new(), false)
        .await
        .map(drop)
}

fn header() -> [u8; HEADER_SIZE] {
    let mut header = [0; HEADER_SIZE];
    header[..MAGIC.len()].copy_from_slice(MAGIC);
    header[MAGIC.len()..].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header
}

enum ShardError {
    /// There is no shard file.
    Absent,
    /// The shard file cannot be read, or is not a shard of this format and version.
    Unreadable,
}

/// The records of the file at `path`, which holds shard `index`.
async fn read_shard(path: &Path, index: u8) -> Result<GrowVec<Record, RECORD_CHUNK>, ShardError> {
    let (file, metadata, head) = match lore_io::IoDriver::global()
        .open_read_head(
            path,
            &lore_io::OpenOptions::new().read(true),
            SHARD_HEAD_READ,
        )
        .await
    {
        Ok(parts) => parts,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(ShardError::Absent),
        Err(_) => return Err(ShardError::Unreadable),
    };
    let file_size = metadata.len() as usize;
    if file_size < HEADER_SIZE
        || head.get(..HEADER_SIZE) != Some(&header()[..])
        || !(file_size - HEADER_SIZE).is_multiple_of(size_of::<Record>())
    {
        return Err(ShardError::Unreadable);
    }
    let count = (file_size - HEADER_SIZE) / size_of::<Record>();
    if count > SHARD_RECORDS_MAX {
        return Err(ShardError::Unreadable);
    }
    let records = if file_size <= head.len() {
        GrowVec::read_from(&mut &head[HEADER_SIZE..], count)
            .map_err(|_unreadable| ShardError::Unreadable)?
    } else {
        // SAFETY: the read fills every byte of the records or fails, and a failure drops them
        // here rather than returning them.
        let records = ShardReadSegments(unsafe { GrowVec::new_unzeroed_with_size(count) });
        file.read_exact_vectored_at(records, HEADER_SIZE as u64)
            .await
            .map_err(|_unreadable| ShardError::Unreadable)?
            .0
    };
    let mut previous: Option<&RecordKey> = None;
    for record in records.iter() {
        if record.key[0] != index || previous.is_some_and(|previous| *previous >= record.key) {
            return Err(ShardError::Unreadable);
        }
        previous = Some(&record.key);
    }
    Ok(records)
}

/// A shard's records as the destination of a read of its file.
struct ShardReadSegments(GrowVec<Record, RECORD_CHUNK>);

impl lore_io::StableBufListMut for ShardReadSegments {
    fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        self.0.byte_segments_mut()
    }
}

/// A shard file's contents for one gathered write: the header, then the records straight from
/// the shard's memory, which the read guard keeps unchanged until the write has the bytes.
struct ShardWriteSegments {
    /// Behind a pointer, since a segment must keep its address when the value moves.
    header: Box<[u8; HEADER_SIZE]>,
    shard: OwnedRwLockReadGuard<Shard>,
}

impl lore_io::StableBufList for ShardWriteSegments {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.header.as_slice()).chain(self.shard.records.byte_segments())
    }
}
