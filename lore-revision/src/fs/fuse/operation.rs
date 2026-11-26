// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The `InstanceOperation` of a FUSE-mounted instance. Every query is answered from the
//! filesystem State, a local-content node from its host file under `write/`, never through the
//! kernel, and a diff against the working tree is a diff of that State. A write runs the
//! provider's own request handlers, as the same request through the kernel would, then drops
//! what the kernel cached of the names and files it changed.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use crossbeam_epoch as epoch;
use crossbeam_epoch::Atomic;
use crossbeam_epoch::Owned;
use crossbeam_epoch::Shared;
use crossbeam_utils::atomic::AtomicCell;
use dashmap::DashMap;
use lore_base::error::FileNotFound;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_error_set::prelude::*;
use lore_fuse::abi::entry::SetattrIn;
use lore_fuse::abi::entry::setattr_valid;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_storage::ContentSource;
use smallvec::SmallVec;

use crate::errors::LocalModifications;
use crate::errors::NotSupported;
use crate::fs::filesystem_provider::CaseLookup;
use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::DirectoryListing;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::ListedEntry;
use crate::fs::filesystem_provider::StaticDispatchDirectoryListing;
use crate::fs::fuse::FuseProviderError;
use crate::fs::fuse::attr::served_file_permissions;
use crate::fs::fuse::filesystem::CommitReading;
use crate::fs::fuse::filesystem::FuseFilesystem;
use crate::fs::fuse::filesystem::LookupCase;
use crate::fs::fuse::filesystem::SettleOwed;
use crate::fs::fuse::filesystem::SpelledEntry;
use crate::fs::fuse::filesystem::WriteStep;
use crate::fs::fuse::filesystem::node_at_path;
use crate::fs::fuse::inode::Name;
use crate::fs::fuse::inode::ROOT_INO;
use crate::fs::fuse::listing::Listing;
use crate::fs::fuse::listing::ListingEntry;
use crate::fs::fuse::listing::entry_flags;
use crate::fs::fuse::local_tree::LocalKind;
use crate::fs::fuse::notifications::Invalidations;
use crate::fs::fuse::notifications::Pending;
use crate::fs::fuse::served::Served;
use crate::fs::hold;
use crate::lore::Hash;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_warn;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeExtFlags;
use crate::node::NodeFileMode;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
use crate::state::NodeMapping;
use crate::state::PathStop;
use crate::state::SerializedBlocks;
use crate::state::State;
use crate::state::Stop;
use crate::util::path::RelativePath;

/// How much of a file `infer_is_diffable` reads.
const INFER_PREFIX: usize = 4096;

/// The most an instance write hands one WRITE handler call.
const WRITE_CHUNK: usize = 1 << 20;

pub struct FuseOperation {
    filesystem: Arc<FuseFilesystem>,
    served: ServedContent,
    /// Counted from the first file served until the operation ends
    /// ([`FuseFilesystem::begin_commit_read`]).
    reading: std::sync::OnceLock<CommitReading>,
    /// The mounted revision as stored, loaded by the first write that compares against it.
    revision: tokio::sync::OnceCell<Arc<State>>,
    /// The deletion list as the last write that asked read it.
    deletions_read: ListedDeletions,
    /// The directories the commit adds or moves, handed over after each walk, in sequence
    /// ([`Self::hand_directories`]).
    handed: parking_lot::Mutex<Vec<RelativePath>>,
    /// The committed State's blocks as its serialize wrote them, held until the settle against
    /// that State returns, so the settle reads them from memory.
    serialized: AtomicCell<Option<Box<SerializedBlocks>>>,
    /// The settle the commit owes, from before its revision may become current
    /// ([`Self::owe_settle`]).
    owed: AtomicCell<Option<Box<SettleOwed>>>,
    /// The time of the files the operation serves from the store again, [`UNREAD`] until read
    /// ([`Self::restore_time`]).
    restored_at: AtomicU64,
    /// The command's context, which holds the write token that writing the records back takes,
    /// set once the operation serves a file from the store again
    /// ([`Self::restore_revision_content`], [`Self::serve_unchanged`]), for [`Self::finalize`].
    restored: parking_lot::Mutex<Option<Arc<RepositoryContext>>>,
    /// The State the first marking diff under the command's write token walked through the
    /// operation, with the command's context, set as that diff begins: from then on a comparison
    /// records each local file it finds holding the content compared, which the operation serves
    /// from the store as it ends ([`Self::serve_unchanged`]).
    serving: std::sync::OnceLock<(Arc<Served>, Arc<RepositoryContext>)>,
    /// The local files comparisons found holding the content compared, each with its address,
    /// recorded as [`InstanceOperation::content_source`] records a file it serves.
    matched: parking_lot::Mutex<Vec<(ServedFile, Address)>>,
    /// The clock of the filesystem holding `write/`, read once, before the operation first
    /// compares a local file, [`UNREAD`] until then ([`Self::compared_floor`]).
    compared_floor: AtomicU64,
}

/// A clock an operation has not read: no filesystem stamps it.
const UNREAD: u64 = u64::MAX;

/// The time `clock` holds, read with `read` where it is [`UNREAD`]: the first time stored wins, so
/// every caller sees the one time. The read is boxed: it runs once per operation, and inline it
/// would grow the future of each caller.
async fn read_once<F: Future<Output = u64>>(clock: &AtomicU64, read: impl FnOnce() -> F) -> u64 {
    let held = clock.load(Ordering::Acquire);
    if held != UNREAD {
        return held;
    }
    let read = Box::pin(read()).await;
    match clock.compare_exchange(UNREAD, read, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => read,
        Err(first) => first,
    }
}

/// The deletion list as one read parsed it, swapped whole once the list has changed: a lookup
/// pins the epoch and loads the pointer, taking no lock; a replaced read is freed once no pinned
/// reader can see it.
#[derive(Default)]
struct ListedDeletions(Atomic<DeletionsRead>);

/// The deletion list's paths as one read found them, sorted case-folded, with the list's change
/// count taken before the read ([`DeletionList::changes`](crate::fs::fuse::deletions::DeletionList::changes)).
#[lore_macro::test_pub]
struct DeletionsRead {
    changes: u64,
    paths: Vec<RelativePath>,
}

impl DeletionsRead {
    #[lore_macro::test_pub]
    fn new(changes: u64, mut paths: Vec<RelativePath>) -> Self {
        paths.sort_unstable_by(|a, b| a.as_lowercase_str().cmp(b.as_lowercase_str()));
        Self { changes, paths }
    }

    /// Whether the paths hold `wanted`, case-folded, or a directory above it.
    #[lore_macro::test_pub]
    fn holds(&self, wanted: &str) -> bool {
        let listed = |path: &str| {
            self.paths
                .binary_search_by(|listed| listed.as_lowercase_str().cmp(path))
                .is_ok()
        };
        listed(wanted)
            || wanted
                .match_indices('/')
                .any(|(at, _)| listed(&wanted[..at]))
    }
}

impl Drop for ListedDeletions {
    fn drop(&mut self) {
        // SAFETY: the operation is being dropped, so no reader holds the read.
        unsafe {
            let read = self
                .0
                .swap(Shared::null(), Ordering::Relaxed, epoch::unprotected());
            if !read.is_null() {
                drop(read.into_owned());
            }
        }
    }
}

/// The files whose content was served through an operation, by their path as the caller spelled
/// it: what a commit stores, and so what it may flip once it has stored them.
#[lore_macro::test_pub]
#[derive(Default)]
pub(crate) struct ServedContent(DashMap<Box<str>, ServedFile>);

/// A file as its content was served, all read before the content: its path as the State spells
/// it, its node and where the node stood, the inode the kernel held for it with that inode's
/// incarnation and write generation, or where none was held the count incarnations and written
/// forgets draw from as it stood (`InodeTable::next_incarnation`), and once the caller stored
/// the content, what it stored and the time the file showed before it was read.
#[lore_macro::test_pub]
#[derive(Clone, Debug)]
pub(crate) struct ServedFile {
    pub spelled: RelativePath,
    pub node: NodeID,
    /// The directory node holding the node, and the node's name hash there.
    pub place: (NodeID, u64),
    pub ino: Option<u64>,
    pub incarnation: u64,
    pub generation: u64,
    pub stored: Option<StoredContent>,
    /// The time the file showed before it was read, which a commit or a serve records for it; 0
    /// until known.
    pub mtime: u64,
}

/// What a commit stored of a file it read: its address, size and mode.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoredContent {
    pub address: Address,
    pub size: u64,
    pub mode: u16,
}

impl ServedContent {
    fn record(&self, path: &RelativePath, file: ServedFile) {
        self.0.insert(path.as_str().into(), file);
    }

    /// Records that the content served for `path`, as it showed `mtime`, was stored as `stored`.
    pub(crate) fn store(&self, path: &RelativePath, stored: StoredContent, mtime: u64) {
        if let Some(mut file) = self.0.get_mut(path.as_str()) {
            file.stored = Some(stored);
            file.mtime = mtime;
        }
    }

    /// Every file recorded whose content was stored.
    pub(crate) fn stored_files(&self) -> Vec<ServedFile> {
        self.0
            .iter()
            .filter(|served| served.stored.is_some())
            .map(|served| served.value().clone())
            .collect()
    }
}

impl ServedFile {
    /// The file `located` names, with `held`, the inode the kernel held for it as read before the
    /// file was located ([`FuseOperation::held_inode`]).
    fn found(located: Located, (ino, incarnation, generation): (Option<u64>, u64, u64)) -> Self {
        Self {
            spelled: located.spelled,
            node: located.node,
            place: (located.dir, located.entry.name_hash),
            ino,
            incarnation,
            generation,
            stored: None,
            mtime: 0,
        }
    }
}

impl FuseOperation {
    pub fn new(filesystem: Arc<FuseFilesystem>) -> Self {
        Self {
            filesystem,
            served: ServedContent::default(),
            reading: std::sync::OnceLock::new(),
            revision: tokio::sync::OnceCell::new(),
            deletions_read: ListedDeletions::default(),
            handed: parking_lot::Mutex::new(Vec::new()),
            serialized: AtomicCell::new(None),
            owed: AtomicCell::new(None),
            restored_at: AtomicU64::new(UNREAD),
            restored: parking_lot::Mutex::new(None),
            serving: std::sync::OnceLock::new(),
            matched: parking_lot::Mutex::new(Vec::new()),
            compared_floor: AtomicU64::new(UNREAD),
        }
    }

    pub fn filesystem(&self) -> &Arc<FuseFilesystem> {
        &self.filesystem
    }

    /// Collects `paths`, directories the commit adds or moves, for its settle.
    pub(crate) fn hand_directories(&self, paths: Vec<RelativePath>) {
        let mut handed = self.handed.lock();
        if handed.is_empty() {
            *handed = paths;
        } else {
            handed.extend(paths);
        }
    }

    /// The directories handed over so far, taken.
    fn take_handed(&self) -> Vec<RelativePath> {
        std::mem::take(&mut *self.handed.lock())
    }

    pub(crate) fn keep_serialized_blocks(&self, blocks: SerializedBlocks) {
        self.serialized.store(Some(Box::new(blocks)));
    }

    pub(crate) fn drop_serialized_blocks(&self) {
        drop(self.serialized.take());
    }

    /// Arms the settle the commit owes with the directories handed over
    /// ([`FuseFilesystem::owe_settle`]). Dropped unreleased with the operation, it leaves
    /// removals recording for the rest of the mount.
    pub(crate) async fn owe_settle(&self) {
        let handed = self.take_handed();
        // Boxed: the lookups would otherwise grow every commit's future.
        let owed = Box::pin(self.filesystem.owe_settle(handed)).await;
        self.owed.store(Some(Box::new(owed)));
    }

    pub(crate) fn release_settle_owed(&self) {
        if let Some(owed) = self.owed.take() {
            owed.release();
        }
    }

    /// Serves from the store the local files a marking diff under the command's write token found
    /// unchanged ([`Self::serve_unchanged`]), then writes back the records holding the time of the
    /// files the operation served from the store again
    /// ([`RecordedTimes::write_back`](crate::fs::fuse::attr::RecordedTimes::write_back)), synced
    /// under `--sync-data`. A failure is logged: the files stay served from the store, and a
    /// remount before the records are written back reports their earlier time. Both are boxed:
    /// they are the cold path, and inline they would grow the future of every command's
    /// operation.
    pub(crate) async fn finalize(&self) {
        if let Some((served, repository)) = self.serving.get()
            && !self.matched.lock().is_empty()
        {
            Box::pin(self.serve_unchanged(repository, served)).await;
        }
        let Some(repository) = self.restored.lock().take() else {
            return;
        };
        if let Err(error) = Box::pin(self.filesystem.recorded_times().write_back(&repository)).await
        {
            lore_warn!(
                "FUSE writing back the time of files served from the store again failed: {error}"
            );
        }
    }

    /// The time the operation records for the files it serves from the store again, read once
    /// ([`read_once`]), as it first serves one, in the staging directory, as a swap reads its
    /// time: a write after any of the files reports a later time.
    async fn restore_time(&self) -> u64 {
        let staging = self
            .filesystem
            .local_files()
            .map(|local| local.staging_dir());
        read_once(&self.restored_at, || {
            crate::state::time_before_next_write(staging)
        })
        .await
    }

    /// The clock of the filesystem holding `write/` as the operation first compared a local file,
    /// read once then ([`read_once`]), in the staging directory: a time older than it was shown
    /// before the operation read the file, and a write after the read carries no older one.
    /// `None` for a mount with no local files.
    async fn compared_floor(&self) -> Option<u64> {
        let staging = self.filesystem.local_files()?.staging_dir();
        Some(
            read_once(&self.compared_floor, || {
                crate::state::read_clock_floor_in(staging)
            })
            .await,
        )
    }

    /// The time the records hold for the local file at `path`, read before a diff compares it, 0
    /// for none, which spares the diff reading the file where it shows that time; first reads the
    /// clock the time a match records must be older than ([`Self::record_compared_time`]). `None`
    /// for a mount with no local files, and for a diff under the command's write token, which
    /// reads the file, as a stage and a commit do; a marking one serves what it finds unchanged
    /// from the store ([`Self::serve_unchanged`]).
    pub(crate) async fn recorded_time(
        &self,
        repository: &RepositoryContext,
        path: &RelativePath,
    ) -> Option<u64> {
        if repository.try_write_token().is_some() {
            return None;
        }
        self.compared_floor().await?;
        Some(
            self.filesystem
                .recorded_times()
                .mtime_for(repository, path, 0)
                .await,
        )
    }

    /// Records `mtime` for `path`, a local file a diff found holding the content of `revision`,
    /// the diff's current revision, where `mtime` is older than the clock read before the file was
    /// read, as a write after the read carries a later time, and the records still hold
    /// `expected`, the time [`Self::recorded_time`] read. Recorded under the swaps lock, taken
    /// only where free, and only where the mount still stands at `revision`: a swap and a commit
    /// set the revision and the times of the files they change under it, so no time is recorded
    /// for content the mounted revision no longer holds. Written back by the next operation that
    /// writes the records back. Takes the path rather than the key the lookup hashed: the key
    /// held across the file's read would grow the future of every comparison, to spare a hash
    /// only where a read matched.
    pub(crate) async fn record_compared_time(
        &self,
        repository: &RepositoryContext,
        path: &RelativePath,
        (expected, mtime): (u64, u64),
        revision: Hash,
    ) {
        let floor = self.compared_floor.load(Ordering::Acquire);
        if floor == UNREAD || mtime >= floor {
            return;
        }
        let Ok(_swaps) = self.filesystem.swap_plan().try_lock() else {
            return;
        };
        if self.filesystem.served().state.revision() == revision {
            self.filesystem
                .recorded_times()
                .replace(repository, path, expected, mtime)
                .await;
        }
    }

    /// The listing entry `path` names, a symlink included.
    async fn entry_at(&self, path: &RelativePath) -> Result<Option<SpelledEntry>, FsError> {
        let served = self.filesystem.served();
        self.filesystem
            .spelled_entry_at_path(&served, path)
            .await
            .map_err(|errno| errno_error(errno, path))
    }

    /// Whether `path` names a symlink, which [`Self::located`] answers as nothing there.
    pub(crate) async fn symlink_at(&self, path: &RelativePath) -> Result<bool, FsError> {
        Ok(self
            .entry_at(path)
            .await?
            .is_some_and(|found| found.entry.flags & entry_flags::SYMLINK != 0))
    }

    /// Whether a write at `path` meets a symlink, at the path or at the nearest ancestor that
    /// stands: the user's, which no command writes over or through.
    pub(crate) async fn symlink_at_or_above(&self, path: &RelativePath) -> Result<bool, FsError> {
        let mut at = path.clone();
        while !at.is_empty() {
            if let Some(found) = self.entry_at(&at).await? {
                return Ok(found.entry.flags & entry_flags::SYMLINK != 0);
            }
            at = at.parent_path();
        }
        Ok(false)
    }

    /// The listing entry `path` names, with where its content lives on the host when it is
    /// local content. A symlink is nothing there, as the listing leaves one out: a command
    /// versions none, as an ordinary clone's walk takes none.
    async fn located(&self, path: &RelativePath) -> Result<Option<Located>, FsError> {
        let found = self
            .entry_at(path)
            .await?
            .filter(|found| found.entry.flags & entry_flags::SYMLINK == 0);
        Ok(found.map(|found| {
            let host = (found.entry.flags & entry_flags::LOCAL != 0)
                .then(|| self.filesystem.host_file_path(&found.spelled))
                .flatten();
            Located {
                entry: found.entry,
                node: found.node,
                dir: found.dir,
                spelled: found.spelled,
                host,
            }
        }))
    }

    /// The inode the kernel holds for `path`, with its incarnation and write generation; where
    /// none is held, the count incarnations and written forgets draw from as it stands
    /// (`InodeTable::next_incarnation`) and generation 0. Read before the file is located, so a
    /// write that lands after the read moves them.
    fn held_inode(&self, path: &RelativePath) -> (Option<u64>, u64, u64) {
        let next = self.filesystem.inodes().next_incarnation();
        self.filesystem
            .live_inode(path)
            .and_then(|ino| {
                self.filesystem
                    .inode_incarnation(ino)
                    .map(|(incarnation, generation)| (Some(ino), incarnation, generation))
            })
            .unwrap_or((None, next, 0))
    }

    /// What the local content at `path` is, from its host file; `None` where `path` names
    /// tracked content or nothing.
    async fn local_info(&self, path: &RelativePath) -> Result<Option<FileInfo>, FsError> {
        let Some(located) = self.located(path).await? else {
            return Ok(None);
        };
        Ok(match located.host {
            None => None,
            Some(_) if is_dir(&located.entry) => Some(FileInfo::Directory),
            Some(host) => Some(
                host_info(&host)
                    .await
                    .map_err(|error| host_error(path, &error))?,
            ),
        })
    }

    /// Where the file at `path` is held. `None` where no file is there.
    async fn file_source(&self, path: &RelativePath) -> Result<Option<FileSource>, FsError> {
        Ok(self
            .located(path)
            .await?
            .filter(|located| !is_dir(&located.entry))
            .map(|located| match located.host {
                Some(host) => FileSource::Local(host),
                None => FileSource::Tracked(located.entry),
            }))
    }

    /// The listing of the directory at `path` in `served`, rebuilt until `valid` accepts it or
    /// the rebuild bound runs out, and where it lives on the host; `None` where `path` holds no
    /// directory.
    async fn directory(
        &self,
        served: &Served,
        path: &RelativePath,
        valid: &(dyn Fn(&Listing) -> bool + Sync),
    ) -> Result<Option<(Arc<Listing>, Option<PathBuf>)>, FsError> {
        Ok(self
            .filesystem
            .spelled_listing_at_path(served, path, valid)
            .await
            .map_err(|errno| errno_error(errno, path))?
            .map(|(listing, spelled)| (listing, self.filesystem.host_file_path(&spelled))))
    }
}

/// `diff.filesystem_path` as the filesystem State spells it where `node` holds it, else as the
/// `from` tree does, so every path a diff builds, and every host path measured from one, uses the
/// State's spelling whatever case the caller named the root in.
async fn state_spelling(
    served: &Served,
    node: NodeID,
    diff: &FilesystemDiffContext,
) -> RelativePath {
    let requested = &diff.filesystem_path;
    if requested.is_empty() {
        return requested.clone();
    }
    let spelled = if node.is_valid_node_id() {
        served
            .state
            .node_path(served.repository.clone(), node)
            .await
    } else if diff.from.node.is_valid_node_id() {
        diff.from
            .state
            .node_path(diff.from.repository.clone(), diff.from.node)
            .await
    } else {
        return requested.clone();
    };
    spelled
        .ok()
        .and_then(|spelled| RelativePath::new_from_initial_path(&spelled).ok())
        .filter(|spelled| spelled.as_lowercase_str() == requested.as_lowercase_str())
        .unwrap_or_else(|| requested.clone())
}

/// A listing entry, its node and path as the State spells it, and where its content lives on
/// the host when it is local content.
struct Located {
    entry: ListingEntry,
    node: NodeID,
    dir: NodeID,
    spelled: RelativePath,
    host: Option<PathBuf>,
}

impl FuseOperation {
    /// What the file at `path` holds, measured from `node`, the record a diff walk read of it:
    /// a local-content node's host file under `write/`, read through a symbolic link as a path
    /// named on its own is, and a tracked node's record. Nothing is resolved through a listing.
    pub(crate) async fn measure(
        &self,
        path: &RelativePath,
        node: &Node,
    ) -> Result<FileInfo, FsError> {
        if node.ext_flags().contains(NodeExtFlags::LocalContent) {
            let Some(host) = self.filesystem.host_file_path(path) else {
                return Ok(FileInfo::NotExist);
            };
            return host_info(&host)
                .await
                .map_err(|error| host_error(path, &error));
        }
        Ok(FileInfo::File {
            executable: Some(node.mode & NodeFileMode::Executable == NodeFileMode::Executable),
            size: node.size,
            mtime: self.filesystem.base_time(),
        })
    }
}

/// Where a file's content is held.
enum FileSource {
    /// The host file under `write/`.
    Local(PathBuf),
    /// The State's entry.
    Tracked(ListingEntry),
}

/// What the host entry at `host` holds, read through a symbolic link as a path named on its
/// own is.
async fn host_info(host: &Path) -> std::io::Result<FileInfo> {
    match lore_io::IoDriver::global().metadata(host).await {
        Ok(metadata) => Ok(FileInfo::from_metadata(&metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileInfo::NotExist),
        Err(error) => Err(error),
    }
}

fn host_error(path: impl std::fmt::Display, error: &std::io::Error) -> FsError {
    FsError::internal(format!("reading '{path}' from write/: {error}"))
}

/// Refuses `action` on a provider without `write/`.
fn read_only<T>(action: &str) -> Result<T, FsError> {
    Err(NotSupported {
        operation: format!("{action} on a read-only instance"),
    }
    .into())
}

fn errno_error(errno: Errno, path: &RelativePath) -> FsError {
    FsError::internal(format!(
        "reading '{path}' from the mounted revision: {}",
        std::io::Error::from_raw_os_error(errno.0)
    ))
}

fn is_dir(entry: &ListingEntry) -> bool {
    entry.flags & entry_flags::IS_DIR != 0
}

fn file_info_of(entry: &ListingEntry, mtime: u64) -> FileInfo {
    if is_dir(entry) {
        FileInfo::Directory
    } else {
        FileInfo::File {
            executable: Some(entry.mode & NodeFileMode::Executable == NodeFileMode::Executable),
            size: entry.size,
            mtime,
        }
    }
}

/// The inodes an instance write looked up, each holding the one lookup it took, which the write
/// gives back when it ends, as the kernel forgets what it looked up.
struct Looked<'a> {
    fs: &'a FuseFilesystem,
    inos: SmallVec<[u64; 8]>,
}

impl<'a> Looked<'a> {
    fn new(fs: &'a FuseFilesystem) -> Self {
        Self {
            fs,
            inos: SmallVec::new(),
        }
    }

    fn hold(&mut self, ino: u64) -> u64 {
        self.inos.push(ino);
        ino
    }
}

impl Drop for Looked<'_> {
    fn drop(&mut self) {
        for &ino in self.inos.iter().rev() {
            self.fs.drop_lookups(ino, 1);
        }
    }
}

/// A file staged under `write-staging/`, removed when dropped.
struct StagedFile(PathBuf);

impl Drop for StagedFile {
    // The provider's own file under `write-staging/`, as a copy-up's `StagedCopy` is.
    #[allow(clippy::disallowed_methods)]
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            lore_warn!("FUSE could not remove '{}': {error}", self.0.display());
        }
    }
}

/// What an instance write puts in a file.
enum Content<'a> {
    Bytes(&'a [u8]),
    /// A host file's bytes.
    Host(&'a Path),
}

fn is_dir_mode(mode: u32) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR
}

/// `result` as an instance write reports it: a refusal (`EROFS`) as [`NotSupported`] of a write at
/// or below a link, which is read-only, as every caller refuses a provider without `write/` first
/// ([`FuseOperation::writable`]).
fn written<T>(result: Result<T, Errno>, action: &str, path: &RelativePath) -> Result<T, FsError> {
    match result {
        Ok(done) => Ok(done),
        Err(Errno::EROFS) => Err(NotSupported {
            operation: format!("{action} at or below a link, which is read-only"),
        }
        .into()),
        Err(errno) => Err(FsError::internal(format!(
            "{action} '{path}': {}",
            std::io::Error::from_raw_os_error(errno.0)
        ))),
    }
}

impl FuseOperation {
    /// Refuses a write on a provider without `write/`, before anything is looked up.
    fn writable(&self, action: &str) -> Result<(), FsError> {
        if self.filesystem.local_files().is_none() {
            return read_only(action);
        }
        Ok(())
    }

    /// The entry `name` in directory `parent`, its inode held in `looked`; `None` where nothing
    /// stands there.
    async fn child(
        &self,
        looked: &mut Looked<'_>,
        parent: u64,
        name: &str,
    ) -> Result<Option<EntryReply>, Errno> {
        let served = self.filesystem.served();
        // Any spelling: lore names a path in the spelling it was given, which may differ in case
        // from the one the mount holds.
        match self
            .filesystem
            .lookup_case_at(&served, parent, name.as_bytes(), LookupCase::Folded)
            .await
        {
            Ok(reply) if reply.ino != 0 => {
                looked.hold(reply.ino);
                Ok(Some(reply))
            }
            Ok(_) | Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno),
        }
    }

    /// The inode of the directory at `path`, made with its missing ancestors where `create`.
    async fn directory_at(
        &self,
        (looked, changed): (&mut Looked<'_>, &mut Invalidations),
        path: &RelativePath,
        create: bool,
    ) -> Result<u64, Errno> {
        let mut ino = ROOT_INO;
        if path.is_empty() {
            return Ok(ino);
        }
        for name in path.as_str().split('/') {
            ino = match self.child(looked, ino, name).await? {
                Some(reply) if is_dir_mode(reply.attr.mode) => reply.ino,
                Some(_) => return Err(Errno::ENOTDIR),
                None if create => {
                    self.made_directory((&mut *looked, &mut *changed), ino, name)
                        .await?
                }
                None => return Err(Errno::ENOENT),
            };
        }
        Ok(ino)
    }

    /// The inode of the directory `name` made in directory inode `parent`, where a lookup found
    /// nothing there. A create that finds the name taken may have met another request's create
    /// between its host entry and its inode, as writes making one directory for their files do:
    /// it waits until no create holds the name before it looks again, so the lookup finds the
    /// other create's inode rather than making one of its own, and it makes the directory itself
    /// where that create failed.
    async fn made_directory(
        &self,
        (looked, changed): (&mut Looked<'_>, &mut Invalidations),
        parent: u64,
        name: &str,
    ) -> Result<u64, Errno> {
        // One round for a create racing others, one for a create that failed meanwhile, one more
        // for a create that raced the retry.
        for _ in 0..3 {
            let made = self
                .filesystem
                .create_node(parent, name.as_bytes(), LocalKind::Dir, (0o755, &[]), true)
                .await
                .map_err(Errno::from);
            match made {
                Ok(made) => {
                    let made = looked.hold(made.ino);
                    entry_changed(changed, parent, name.as_bytes());
                    return Ok(made);
                }
                // Made meanwhile by another writer, or being made.
                Err(Errno::EEXIST) => self.filesystem.creates_finished(parent, name).await,
                Err(errno) => return Err(errno),
            }
            match self.child(looked, parent, name).await? {
                Some(reply) if is_dir_mode(reply.attr.mode) => return Ok(reply.ino),
                Some(_) => return Err(Errno::ENOTDIR),
                None => {}
            }
        }
        Err(Errno::EEXIST)
    }

    /// The file at `path` with its mode, emptied, or created empty with the directories above
    /// it where nothing stands there or a symlink did, which is taken away rather than written
    /// through.
    async fn emptied_file(
        &self,
        (looked, changed): (&mut Looked<'_>, &mut Invalidations),
        path: &RelativePath,
    ) -> Result<(u64, u32), Errno> {
        let parent = self
            .directory_at((&mut *looked, &mut *changed), &path.parent_path(), true)
            .await?;
        let name = path.name();
        // Two rounds for a create racing another writer's, one more for a symlink taken away.
        for _ in 0..3 {
            match self.child(looked, parent, name).await? {
                Some(reply) if is_dir_mode(reply.attr.mode) => return Err(Errno::EISDIR),
                Some(reply) if reply.attr.mode & libc::S_IFMT == libc::S_IFLNK => {
                    match self
                        .filesystem
                        .remove_node(parent, name.as_bytes(), false, true)
                        .await
                        .map_err(Errno::from)
                    {
                        // Removed meanwhile by another writer.
                        Ok(()) | Err(Errno::ENOENT) => {}
                        Err(errno) => return Err(errno),
                    }
                    entry_changed(changed, parent, name.as_bytes());
                }
                Some(reply) => {
                    let empty = SetattrIn {
                        valid: setattr_valid::SIZE,
                        ..SetattrIn::default()
                    };
                    self.filesystem
                        .setattr_node(reply.ino, &empty, true)
                        .await?;
                    return Ok((reply.ino, reply.attr.mode));
                }
                None => {
                    self.filesystem
                        .write_step_allowed(WriteStep::InstanceFoundNothing { parent });
                    let created = self
                        .filesystem
                        .create_file_node(parent, name.as_bytes(), libc::S_IFREG | 0o644, true)
                        .await
                        .map_err(Errno::from);
                    match created {
                        Ok(created) => {
                            let entry = created.entry;
                            let ino = looked.hold(entry.ino);
                            entry_changed(changed, parent, name.as_bytes());
                            return Ok((ino, entry.attr.mode));
                        }
                        // Created meanwhile by another writer: emptied as found.
                        Err(Errno::EEXIST) => {}
                        Err(errno) => return Err(errno),
                    }
                }
            }
        }
        Err(Errno::EEXIST)
    }

    /// Writes `content` over the file at `path`, as a user's truncate and writes do, then sets
    /// its executable bit where `executable` names one.
    async fn write_content(
        &self,
        changed: &mut Invalidations,
        path: &RelativePath,
        content: Content<'_>,
        executable: Option<bool>,
    ) -> Result<(), Errno> {
        let mut looked = Looked::new(&self.filesystem);
        let (ino, mode) = self
            .emptied_file((&mut looked, &mut *changed), path)
            .await?;
        match content {
            Content::Bytes(bytes) => self.write_at(ino, 0, bytes).await?,
            Content::Host(host) => self.write_host(ino, host).await?,
        }
        if let Some(executable) = executable {
            self.set_executable(ino, mode, executable).await?;
        }
        changed.inodes.push(ino);
        Ok(())
    }

    async fn write_at(&self, ino: u64, offset: u64, bytes: &[u8]) -> Result<(), Errno> {
        let mut done = 0;
        while done < bytes.len() {
            let end = bytes.len().min(done + WRITE_CHUNK);
            let wrote = self
                .filesystem
                .write_node(ino, offset + done as u64, &bytes[done..end], true)
                .await?;
            if wrote == 0 {
                return Err(Errno::EIO);
            }
            done += wrote;
        }
        Ok(())
    }

    /// Writes the bytes of host file `host` into file `ino`, a chunk at a time.
    async fn write_host(&self, ino: u64, host: &Path) -> Result<(), Errno> {
        use std::os::unix::fs::FileExt as _;

        let file = std::fs::File::open(host).map_err(|error| io_errno(&error))?;
        let size = file.metadata().map_err(|error| io_errno(&error))?.len();
        let mut buffer =
            vec![0u8; usize::try_from(size).map_or(WRITE_CHUNK, |size| size.min(WRITE_CHUNK))];
        let mut offset = 0;
        while offset < size {
            let read = file
                .read_at(&mut buffer, offset)
                .map_err(|error| io_errno(&error))?;
            if read == 0 {
                break;
            }
            self.write_at(ino, offset, &buffer[..read]).await?;
            offset += read as u64;
        }
        Ok(())
    }

    /// Sets file `ino`'s executable bits, `mode` being its mode now.
    async fn set_executable(&self, ino: u64, mode: u32, executable: bool) -> Result<(), Errno> {
        let wanted = if executable {
            mode | 0o111
        } else {
            mode & !0o111
        };
        if wanted == mode {
            return Ok(());
        }
        let attr = SetattrIn {
            valid: setattr_valid::MODE,
            mode: wanted,
            ..SetattrIn::default()
        };
        self.filesystem.setattr_node(ino, &attr, true).await?;
        Ok(())
    }

    /// The content `address` names, staged under `write-staging/`, with the fragment it came
    /// from.
    async fn stage_stored(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
    ) -> Result<(StagedFile, Fragment), FsError> {
        let Some(staging) = self.filesystem.instance_staging_path() else {
            return read_only("writing a file");
        };
        let staged = StagedFile(staging);
        let options = crate::immutable::read_options_from_repository(&repository);
        let (fragment, _) = self
            .filesystem
            .scoped(crate::immutable::read_into_file(
                repository, address, &staged.0, None, options,
            ))
            .await
            .forward_any_with::<FsError, _>(|| format!("reading the content of '{path}'"))?;
        Ok((staged, fragment))
    }

    /// Gives the file at `path` `node`'s content as tracked content served from the store, as a
    /// forced sync overwrites a local file, where the State holds the mounted revision's own node
    /// there and the revision holds that content: nothing is copied, and `write/` keeps nothing of
    /// it. A node made at the path since, by a create or a rename onto it, stands over a deletion
    /// the list records, which only its local copy survives at the next mount. Answers the time
    /// the file reports where it did ([`Self::restore_time`]), set in the records before the file
    /// is served from the store, so an inode made of it meanwhile reports it, and written back as
    /// the operation ends.
    async fn restore_revision_content(
        &self,
        changed: &mut Invalidations,
        repository: &Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<Option<u64>, FsError> {
        let Some(located) = self.located(path).await? else {
            return Ok(None);
        };
        if is_dir(&located.entry) {
            return Ok(None);
        }
        let served = self.filesystem.served();
        let Some(revision) = self.mounted_revision(&served).await? else {
            return Ok(None);
        };
        let Some(held) =
            Self::revision_record(revision, &served.repository, &located.spelled, located.node)
                .await?
        else {
            return Ok(None);
        };
        if !held.is_file()
            || held.address.hash != node.address.hash
            || held.size != node.size
            || held.mode != node.mode
            || self.deletion_listed(&located.spelled)
        {
            return Ok(None);
        }
        let target = StoredContent {
            address: held.address,
            size: held.size,
            mode: held.mode,
        };
        let Some(mtime) = self
            .serve_restored(repository, &served, &located, &target)
            .await
            .forward_any_with::<FsError, _>(|| format!("restoring '{path}' to the revision"))?
        else {
            return Ok(None);
        };
        if let Some(ino) = self.filesystem.live_inode(&located.spelled) {
            changed.inodes.push(ino);
        }
        self.restored
            .lock()
            .get_or_insert_with(|| repository.clone());
        Ok(Some(mtime))
    }

    /// The mounted revision, loaded once per operation, where `served` stands at it.
    async fn mounted_revision(&self, served: &Served) -> Result<Option<&State>, FsError> {
        let revision = self
            .revision
            .get_or_try_init(|| {
                crate::fs::fuse::filesystem::load_revision(&served.repository, served.revision())
            })
            .await
            .forward_any::<FsError>("loading the mounted revision")?;
        Ok((revision.revision() == served.revision()).then_some(&**revision))
    }

    /// The record `revision` holds for `node` at `spelled`, where it holds that node there, found
    /// as a lookup finds a name, whatever its case.
    async fn revision_record(
        revision: &State,
        repository: &Arc<RepositoryContext>,
        spelled: &RelativePath,
        node: NodeID,
    ) -> Result<Option<Node>, FsError> {
        match revision
            .locate_node(repository.clone(), spelled.as_str())
            .await
        {
            Ok(found) if found.own_node() == Some(node) => revision
                .node(repository.clone(), node)
                .await
                .map(Some)
                .forward_any::<FsError>("reading the mounted revision"),
            _ => Ok(None),
        }
    }

    /// Sets the operation's time ([`Self::restore_time`]) for the file `located` names in the
    /// records, where a lookup sees it at once, then gives the file `target` over its local content
    /// ([`FuseFilesystem::overwrite_local_file`]), and answers the time where the file changed. Its
    /// own future, so [`Self::restore_revision_content`] holds its locals across one await.
    async fn serve_restored(
        &self,
        repository: &RepositoryContext,
        served: &Served,
        located: &Located,
        target: &StoredContent,
    ) -> Result<Option<u64>, FuseProviderError> {
        let mtime = self.restore_time().await;
        self.filesystem
            .recorded_times()
            .set(repository, &located.spelled, mtime)
            .await;
        let changed = self
            .filesystem
            .overwrite_local_file(
                served,
                &located.spelled,
                located.node,
                (located.dir, located.entry.name_hash),
                target,
                mtime,
            )
            .await?;
        Ok(changed.then_some(mtime))
    }

    /// Serves from the store each local file the operation's comparisons found holding the
    /// content compared ([`Self::file_holds_content`]) since a marking diff under the command's
    /// write token walked `served`, where that is what the mounted revision holds for its node
    /// ([`Self::revision_content`]) and `served` is still the State served: the time the file
    /// showed before the comparison read it is set in the records, where a lookup sees it at
    /// once, then the file is flipped as
    /// a commit flips a file it stored ([`FuseFilesystem::serve_unchanged`]), and the records are
    /// written back as the operation ends. A file a write changed since its read stays local, as
    /// does one whose check fails, which is logged. Runs once, as the operation ends.
    async fn serve_unchanged(&self, repository: &Arc<RepositoryContext>, served: &Arc<Served>) {
        let matched = std::mem::take(&mut *self.matched.lock());
        if !Arc::ptr_eq(&self.filesystem.served(), served) {
            return;
        }
        let revision = match self.mounted_revision(served).await {
            Ok(Some(revision)) => revision,
            Ok(None) => return,
            Err(error) => {
                lore_warn!("FUSE serving unchanged local files from the store failed: {error}");
                return;
            }
        };
        let mut files = Vec::with_capacity(matched.len());
        for (mut file, address) in matched {
            match self
                .revision_content((revision, &served.repository), &file, address)
                .await
            {
                Ok(Some(stored)) => {
                    self.restored
                        .lock()
                        .get_or_insert_with(|| repository.clone());
                    self.filesystem
                        .recorded_times()
                        .set(repository, &file.spelled, file.mtime)
                        .await;
                    file.stored = Some(stored);
                    files.push(file);
                }
                Ok(None) => {}
                Err(error) => lore_warn!(
                    "FUSE checking '{}' against the mounted revision failed: {error}",
                    file.spelled
                ),
            }
        }
        if files.is_empty() {
            return;
        }
        let flipped = self
            .filesystem
            .serve_unchanged(&files, served, revision)
            .await;
        lore_debug!(
            "FUSE served {flipped} of {} unchanged local files from the store",
            files.len()
        );
    }

    /// What `revision` holds for the node of `file`, a local file found holding the content at
    /// `address`: `None` where the revision holds another node at the path, spells the path
    /// otherwise, or holds other content or size, where the file shows another time than it did
    /// as it was read or other permissions than a file served from the store reports, or where
    /// the deletion list holds the path or a directory above it.
    async fn revision_content(
        &self,
        (revision, repository): (&State, &Arc<RepositoryContext>),
        file: &ServedFile,
        address: Address,
    ) -> Result<Option<StoredContent>, FsError> {
        if self.deletion_listed(&file.spelled) {
            return Ok(None);
        }
        let Some(held) =
            Self::revision_record(revision, repository, &file.spelled, file.node).await?
        else {
            return Ok(None);
        };
        if !held.is_file() || held.address.hash != address.hash {
            return Ok(None);
        }
        let spelled = revision
            .node_path(repository.clone(), file.node)
            .await
            .forward_any::<FsError>("reading the mounted revision")?;
        if spelled != file.spelled.as_str() {
            return Ok(None);
        }
        let Some(host) = self.filesystem.host_file_path(&file.spelled) else {
            return Ok(None);
        };
        let stat = match lore_io::IoDriver::global().metadata(&host).await {
            Ok(metadata) => lore_io::Stat::from(&metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(host_error(&file.spelled, &error)),
        };
        let executable = held.mode & NodeFileMode::Executable == NodeFileMode::Executable;
        if stat.kind != lore_io::StatKind::File
            || stat.len != held.size
            || stat.mode.map(|mode| mode & 0o7777) != Some(served_file_permissions(executable))
            || crate::util::fs::modified_millis(stat.modified) != file.mtime
        {
            return Ok(None);
        }
        Ok(Some(StoredContent {
            address: held.address,
            size: held.size,
            mode: held.mode,
        }))
    }

    /// Whether the deletion list holds `path` or a directory above it, which the next mount
    /// detaches unless a copy under `write/` stands there: a node moved away and back keeps its
    /// id over the deletion its move recorded. The list is read once per operation, and again
    /// only where a write has appended to it since.
    fn deletion_listed(&self, path: &RelativePath) -> bool {
        let Some(deletions) = self.filesystem.deletions() else {
            return false;
        };
        let wanted = path.as_lowercase_str();
        // Taken before the read, so the read holds every path appended until then.
        let changes = deletions.changes();
        {
            let guard = epoch::pin();
            // SAFETY: loaded under `guard`; a replaced read is freed only through `defer_destroy`.
            let kept = unsafe {
                self.deletions_read
                    .0
                    .load(Ordering::Acquire, &guard)
                    .as_ref()
            };
            if let Some(read) = kept
                && read.changes == changes
            {
                return read.holds(wanted);
            }
        }
        // Read and sorted unpinned, so the epoch moves on meanwhile.
        let Ok(paths) = deletions.read() else {
            // Unread, the list may hold it: the write path is right either way.
            return true;
        };
        let read = DeletionsRead::new(changes, paths);
        let listed = read.holds(wanted);
        let guard = epoch::pin();
        let old = self
            .deletions_read
            .0
            .swap(Owned::new(read), Ordering::AcqRel, &guard);
        if !old.is_null() {
            // SAFETY: `old` was just unlinked, so no later load returns it; pinned readers keep
            // it alive until they unpin.
            unsafe { guard.defer_destroy(old) };
        }
        listed
    }

    /// Removes what stands at `path`, a directory with everything below it where `recursive`.
    /// Nothing standing there is what was asked for, as on an ordinary clone.
    async fn remove_at(
        &self,
        changed: &mut Invalidations,
        path: &RelativePath,
        recursive: bool,
    ) -> Result<(), Errno> {
        let mut looked = Looked::new(&self.filesystem);
        let parent = match self
            .directory_at((&mut looked, &mut *changed), &path.parent_path(), false)
            .await
        {
            Err(Errno::ENOENT | Errno::ENOTDIR) => return Ok(()),
            parent => parent?,
        };
        let name = path.name();
        let Some(reply) = self.child(&mut looked, parent, name).await? else {
            return Ok(());
        };
        let dir = is_dir_mode(reply.attr.mode);
        if dir && recursive {
            for child in self.child_names(path).await? {
                Box::pin(self.remove_at(changed, &path.join(&child), true)).await?;
            }
        }
        match self
            .filesystem
            .remove_node(parent, name.as_bytes(), dir, true)
            .await
            .map_err(Errno::from)
        {
            Ok(()) | Err(Errno::ENOENT) => {}
            Err(errno) => return Err(errno),
        }
        entry_changed(changed, parent, name.as_bytes());
        Ok(())
    }

    /// Removes the directory at `path`, failing as `rmdir` does where nothing stands there, where a
    /// file does, or where the directory holds entries.
    async fn remove_empty_directory_at(
        &self,
        changed: &mut Invalidations,
        path: &RelativePath,
    ) -> Result<(), Errno> {
        let mut looked = Looked::new(&self.filesystem);
        let parent = self
            .directory_at((&mut looked, &mut *changed), &path.parent_path(), false)
            .await?;
        let name = path.name();
        self.filesystem
            .remove_node(parent, name.as_bytes(), true, true)
            .await
            .map_err(Errno::from)?;
        entry_changed(changed, parent, name.as_bytes());
        Ok(())
    }

    /// The names the directory at `path` holds.
    async fn child_names(&self, path: &RelativePath) -> Result<Vec<String>, Errno> {
        let mut listing = InstanceOperation::read_directory(self, path)
            .await
            .map_err(|_error| Errno::EIO)?;
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            names.push(entry.map_err(|_error| Errno::EIO)?.name);
        }
        Ok(names)
    }

    /// Moves what stands at `from` to `to`, a directory onto a directory child by child.
    async fn rename_at(
        &self,
        changed: &mut Invalidations,
        from: &RelativePath,
        to: &RelativePath,
    ) -> Result<(), Errno> {
        let mut looked = Looked::new(&self.filesystem);
        let from_parent = self
            .directory_at((&mut looked, &mut *changed), &from.parent_path(), false)
            .await?;
        let source = self
            .child(&mut looked, from_parent, from.name())
            .await?
            .ok_or(Errno::ENOENT)?;
        let to_parent = self
            .directory_at((&mut looked, &mut *changed), &to.parent_path(), true)
            .await?;
        let moved = self
            .filesystem
            .rename_node(
                (from_parent, from.name().as_bytes()),
                (to_parent, to.name().as_bytes()),
                0,
                true,
            )
            .await
            .map_err(Errno::from);
        match moved {
            Ok(()) => {}
            // Onto a directory, or a tracked directory, which RENAME refuses to move whole: child
            // by child. A change of case alone has no other directory to move into.
            Err(Errno::ENOTEMPTY | Errno::EEXIST | Errno::EXDEV)
                if is_dir_mode(source.attr.mode)
                    && from.as_lowercase_str() != to.as_lowercase_str() =>
            {
                self.directory_at((&mut looked, &mut *changed), to, true)
                    .await?;
                for child in self.child_names(from).await? {
                    Box::pin(self.rename_at(changed, &from.join(&child), &to.join(&child))).await?;
                }
                self.filesystem
                    .remove_node(from_parent, from.name().as_bytes(), true, true)
                    .await?;
            }
            Err(errno) => return Err(errno),
        }
        entry_changed(changed, from_parent, from.name().as_bytes());
        entry_changed(changed, to_parent, to.name().as_bytes());
        Ok(())
    }

    /// Serves `link`, a link node of the instance's own tree, at `path`, in place of what stands
    /// there, or takes away the link standing at `path` where `None`: the mount serves a link from
    /// its node (ADR-00036). A link placed over a local change, at or below `path` or a local file
    /// above it, is refused with [`LocalModifications`] unless `force`, which removes the change
    /// first: the local file above, or what stands at `path`. A path through a link takes nothing:
    /// its pin serves it.
    pub(crate) async fn place_link(
        &self,
        path: &RelativePath,
        link: Option<&Node>,
        force: bool,
    ) -> Result<(), FsError> {
        const ACTION: &str = "placing a link";
        self.writable(ACTION)?;
        let served = self.filesystem.served();
        let replaced = if link.is_some() {
            hold::Resolver::new((&served.repository, &served.state))
                .replaced_by_link(path)
                .await
                .forward_any::<FsError>("checking the path of a link for local changes")?
        } else {
            let standing = node_at_path(&served.state, &served.repository, path.as_str())
                .await
                .forward_any::<FsError>("reading the link taken away")?;
            // No link to take away, as below another link, whose pin serves the path: no inode
            // is walked through it.
            if !standing.is_some_and(|(_, node)| node.is_link()) {
                return Ok(());
            }
            None
        };
        drop(served);
        if let Some(replaced) = replaced {
            if !force {
                lore_error!("{path} has local changes the link placed there replaces");
                return Err(LocalModifications.into());
            }
            InstanceOperation::remove_recursive(self, &replaced).await?;
        }
        self.place_at(path, hold::Placing::Link(link), ACTION).await
    }

    /// Serves at `path` a copy of the node `side` names, with every node below it, where nothing
    /// stands there ([`hold::place_copy`]), as a reset puts back the committed directory a link
    /// staged in its place took away. A path through a link takes nothing: its pin serves it.
    pub(crate) async fn place_copy(
        &self,
        path: &RelativePath,
        side: &NodeMapping,
    ) -> Result<(), FsError> {
        const ACTION: &str = "placing a committed node";
        self.writable(ACTION)?;
        self.place_at(path, hold::Placing::Copy(side), ACTION).await
    }

    /// [`Self::place_link`] of the link standing at `path` moved to the revision `pin`, mounting
    /// its node `source`; nothing where no link stands there. A clean link stands there, so no
    /// local change is looked for.
    pub(crate) async fn repin_link(
        &self,
        path: &RelativePath,
        pin: Hash,
        source: NodeID,
    ) -> Result<(), FsError> {
        const ACTION: &str = "moving a link to a pin";
        self.writable(ACTION)?;
        let served = self.filesystem.served();
        let held = node_at_path(&served.state, &served.repository, path.as_str())
            .await
            .forward_any::<FsError>("reading the link a pin moves")?;
        drop(served);
        let Some((_, mut moved)) = held.filter(|(_, held)| held.is_link()) else {
            return Ok(());
        };
        moved.address.hash = pin;
        moved.child = source;
        self.place_at(path, hold::Placing::Link(Some(&moved)), ACTION)
            .await
    }

    /// Places `placing` at `path` for `action`, once local changes are settled, and sends the
    /// kernel what changed: [`Self::place_link`], [`Self::place_copy`] and [`Self::repin_link`].
    /// A path through a link takes nothing: its pin serves it.
    async fn place_at(
        &self,
        path: &RelativePath,
        placing: hold::Placing<'_>,
        action: &str,
    ) -> Result<(), FsError> {
        let takes_away = matches!(placing, hold::Placing::Link(None));
        let mut changed = self.changes();
        let batch = &mut changed.batch;
        let placed = Box::pin(async {
            let mut looked = Looked::new(&self.filesystem);
            let parent = match self
                .directory_at((&mut looked, &mut *batch), &path.parent_path(), !takes_away)
                .await
            {
                Err(Errno::ENOENT | Errno::ENOTDIR) if takes_away => return Ok(()),
                // Inside a link, whose pin serves the path.
                Err(Errno::EROFS) => return Ok(()),
                parent => parent?,
            };
            let name = path.name();
            if self.filesystem.place(parent, name, placing).await? {
                entry_changed(batch, parent, name.as_bytes());
            }
            Ok::<_, Errno>(())
        })
        .await;
        changed.send().await;
        written(placed, action, path)
    }

    /// What an instance write changes, sent from the mount's notification thread in one batch
    /// once it is done, or queued without waiting if the write is dropped first.
    fn changes(&self) -> Pending<'_> {
        Pending::new(self.filesystem.notifications(), Invalidations::default())
    }
}

/// Records that the entry `name` in directory `parent` changed: the kernel drops what it cached
/// of it and the directory's listing. An entry invalidation of a name the kernel never looked up
/// leaves a cached listing as it was.
fn entry_changed(changed: &mut Invalidations, parent: u64, name: &[u8]) {
    changed.entries.push((parent, Name::from_slice(name)));
    changed.inodes.push(parent);
}

fn io_errno(error: &std::io::Error) -> Errno {
    Errno(error.raw_os_error().unwrap_or(libc::EIO))
}

impl FuseOperation {
    /// What the file at `path` is, as [`InstanceOperation::file_info`] reports it; nothing there
    /// where `as_spelled` asks for `path`'s spelling and the State spells it otherwise.
    async fn info_at(&self, path: &RelativePath, as_spelled: bool) -> Result<FileInfo, FsError> {
        if path.is_empty() {
            return Ok(FileInfo::Directory);
        }
        let Some(located) = self
            .located(path)
            .await?
            .filter(|located| !as_spelled || located.spelled.as_str() == path.as_str())
        else {
            return Ok(FileInfo::NotExist);
        };
        let entry = located.entry;
        if is_dir(&entry) {
            return Ok(FileInfo::Directory);
        }
        if let Some(host) = located.host {
            return host_info(&host)
                .await
                .map_err(|error| host_error(path, &error));
        }
        let served = self.filesystem.served();
        let mtime = self
            .filesystem
            .scoped(self.filesystem.recorded_times().mtime_for(
                &served.repository,
                path,
                self.filesystem.base_time(),
            ))
            .await;
        Ok(file_info_of(&entry, mtime))
    }
}

impl InstanceOperation for FuseOperation {
    /// Diffs the filesystem State at `diff.filesystem_path` against `diff.from` by address,
    /// descending only where either side is dirty, and marks `diff.from` only as the intent asks.
    /// The State is read under the filter of `diff.from`'s repository, the command's, as an
    /// ordinary walk lists the working tree under it: the ignore rules a command reads are its
    /// own, where the provider's context holds those of the last revision it held. After a marking
    /// diff under the command's write token, the operation serves from the store, as it ends, the
    /// local files its comparisons found holding the mounted revision's content
    /// ([`FuseOperation::serve_unchanged`]). Nothing at or below a link is reported, as no write
    /// changes a link: not a diff of a linked repository, nor one at a path through a link.
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        if diff.from.repository.is_link() {
            return ChangeStream::nothing();
        }
        if diff.intent.marks_dirty()
            && diff.from.repository.try_write_token().is_some()
            && !diff.operation.dry_run()
        {
            self.serving
                .get_or_init(|| (self.filesystem.served(), diff.from.repository.clone()));
        }
        let filesystem = self.filesystem.clone();
        ChangeStream::spawn(async move |changes| {
            let served = filesystem.served();
            let node = match served
                .state
                .find_to_link::<true>(
                    &served.repository,
                    ROOT_NODE,
                    diff.filesystem_path.as_str(),
                    true,
                )
                .await
            {
                Ok(PathStop {
                    at: Stop::Link(_), ..
                }) => return Ok(FilesystemDiffStats::default()),
                Ok(PathStop {
                    at: Stop::End,
                    node,
                    ..
                }) => node,
                Ok(PathStop {
                    at: Stop::Missing, ..
                })
                | Err(_) => INVALID_NODE,
            };
            let spelled = state_spelling(&served, node, &diff).await;
            let to = NodeMapping {
                node,
                repository: filesystem.provider_repository(&diff.from.repository),
                state: served.state.clone(),
                path: spelled.clone(),
            };
            let mut from = diff.from;
            from.path = spelled.clone();
            let mut current = diff.current;
            current.path = spelled.clone();
            // Boxed: the walk makes a large future, one per diff.
            filesystem
                .scoped(Box::pin(crate::state::diff_states_into(
                    diff.operation,
                    from,
                    current,
                    to,
                    spelled,
                    diff.filter_mode,
                    diff.intent,
                    diff.discards,
                    &changes,
                )))
                .await
        })
    }

    fn file_info(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send {
        self.info_at(path, false)
    }

    /// The served State's index answers a path in any case, so the spelling it holds is compared.
    fn file_info_as_spelled(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send {
        self.info_at(path, true)
    }

    async fn untracked_file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        Ok(self.local_info(path).await?.unwrap_or(FileInfo::NotExist))
    }

    /// [`CaseLookup::PerName`]: the served State's index answers for a name.
    fn case_lookup(&self) -> CaseLookup {
        CaseLookup::PerName
    }

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        if path.is_empty() {
            return Some(true);
        }
        let served = self.filesystem.served();
        let name = path.name();
        let child = self
            .filesystem
            .child_in_directory(&served, &path.parent_path(), name)
            .await
            .ok()??;
        Some(child.is_some_and(|child| child.spelled.as_slice() == name.as_bytes()))
    }

    async fn names_folding_to(
        &self,
        path: &RelativePath,
        name: &str,
    ) -> Result<Vec<String>, FsError> {
        let served = self.filesystem.served();
        let child = self
            .filesystem
            .child_in_directory(&served, path, name)
            .await
            .map_err(|errno| errno_error(errno, path))?;
        Ok(child
            .flatten()
            .and_then(|child| String::from_utf8(child.spelled.to_vec()).ok())
            .into_iter()
            .collect())
    }

    async fn read_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        let served = self.filesystem.served();
        let Some((listing, host)) = self.directory(&served, path, &Listing::is_current).await?
        else {
            return Err(errno_error(Errno::ENOTDIR, path));
        };
        let base_time = self.filesystem.base_time();
        let times = self.filesystem.recorded_times();
        let mtimes = if times.none() {
            None
        } else {
            let mut mtimes = Vec::with_capacity(listing.len());
            for index in 0..listing.len() {
                let name = std::str::from_utf8(listing.name(index));
                let listed = listing.current_entry(index).is_some_and(|entry| {
                    entry.flags & (entry_flags::VIEW_EXCLUDED | entry_flags::LOCAL) == 0
                        && !is_dir(&entry)
                });
                mtimes.push(match name {
                    Ok(name) if listed => {
                        self.filesystem
                            .scoped(times.mtime_for(
                                &served.repository,
                                &path.join(name),
                                base_time,
                            ))
                            .await
                    }
                    _ => base_time,
                });
            }
            Some(mtimes.into_boxed_slice())
        };
        Ok(DirectoryListing::new(StaticDispatchDirectoryListing::Fuse(
            FuseDirectoryListing {
                listing,
                index: 0,
                host,
                base_time,
                mtimes,
            },
        )))
    }

    /// Records the file served (`ServedContent`), its inode's incarnation and write generation
    /// read before the file is resolved, so a write that lands after the read moves them.
    async fn content_source(&self, path: &RelativePath) -> Result<ContentSource<'static>, FsError> {
        self.filesystem
            .write_step_allowed(WriteStep::ContentServing);
        self.reading
            .get_or_init(|| self.filesystem.begin_commit_read());
        let held = self.held_inode(path);
        let Some(mut located) = self
            .located(path)
            .await?
            .filter(|located| !is_dir(&located.entry))
        else {
            return Err(FileNotFound {
                resource: path.to_string(),
            }
            .into());
        };
        let source = match located.host.take() {
            Some(host) => ContentSource::owned_file(host),
            None => ContentSource::virtual_immutable(located.entry.address, located.entry.size),
        };
        self.served.record(path, ServedFile::found(located, held));
        Ok(source)
    }

    /// A local file is measured against the stored object as a real file is; a tracked file
    /// holds its State address. Once a marking diff under the command's write token has begun
    /// ([`Self::serve_unchanged`]), a local file that matches is recorded with its inode as read
    /// before the file was located and `mtime`, the time the caller read for its host file before
    /// asking, where it read one older than the operation's clock ([`Self::compared_floor`]): a
    /// write after the read carries no older time.
    async fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        mtime: u64,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<NodeComparison, FsError> {
        let read = match self.serving.get() {
            Some(_) if mtime != 0 => self
                .compared_floor()
                .await
                .is_some_and(|floor| mtime < floor)
                .then(|| self.held_inode(path)),
            _ => None,
        };
        let Some(mut located) = self
            .located(path)
            .await?
            .filter(|located| !is_dir(&located.entry))
        else {
            return Ok(NodeComparison::Differs);
        };
        let Some(host) = located.host.take() else {
            let entry = &located.entry;
            return Ok(
                if entry.address.hash == previous.hash && entry.size == previous_size {
                    NodeComparison::Matches
                } else {
                    NodeComparison::Differs
                },
            );
        };
        let found = read.map(|held| ServedFile {
            mtime,
            ..ServedFile::found(located, held)
        });
        let matched = crate::immutable::file_matches(
            repository,
            previous,
            Some(previous_size as usize),
            &ContentSource::owned_file(host),
            established,
        )
        .await
        .forward_any::<FsError>("comparing a local file to stored content")?;
        let comparison = crate::state::node_comparison(matched);
        if let Some(found) = found
            && matches!(comparison, NodeComparison::Matches)
        {
            self.matched.lock().push((found, previous));
        }
        Ok(comparison)
    }

    async fn make_executable(&self, path: &RelativePath, executable: bool) -> Result<(), FsError> {
        const ACTION: &str = "making a file executable";
        self.writable(ACTION)?;
        let mut looked = Looked::new(&self.filesystem);
        let mut changed = self.changes();
        // Boxed, as is each write below: the FUSE arm would make every caller's future large.
        let done = Box::pin(async {
            let parent = self
                .directory_at(
                    (&mut looked, &mut changed.batch),
                    &path.parent_path(),
                    false,
                )
                .await?;
            let reply = self
                .child(&mut looked, parent, path.name())
                .await?
                .ok_or(Errno::ENOENT)?;
            if is_dir_mode(reply.attr.mode) {
                return Err(Errno::EISDIR);
            }
            self.set_executable(reply.ino, reply.attr.mode, executable)
                .await?;
            changed.batch.inodes.push(reply.ino);
            Ok(())
        })
        .await;
        changed.send().await;
        written(done, ACTION, path)
    }

    async fn create_dir_all(&self, path: &RelativePath) -> Result<(), FsError> {
        const ACTION: &str = "creating a directory";
        self.writable(ACTION)?;
        let mut looked = Looked::new(&self.filesystem);
        let mut changed = self.changes();
        let made = Box::pin(self.directory_at((&mut looked, &mut changed.batch), path, true)).await;
        changed.send().await;
        written(made.map(|_| ()), ACTION, path)
    }

    async fn write_file(&self, path: &RelativePath, contents: Bytes) -> Result<(), FsError> {
        const ACTION: &str = "writing a file";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let wrote =
            Box::pin(self.write_content(&mut changed.batch, path, Content::Bytes(&contents), None))
                .await;
        changed.send().await;
        written(wrote, ACTION, path)
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        const ACTION: &str = "renaming";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let renamed = Box::pin(self.rename_at(&mut changed.batch, from, to)).await;
        changed.send().await;
        written(renamed, ACTION, from)
    }

    /// Both name the same entry of the served tree, which finds a child by any spelling of its
    /// name: their paths as the State spells them are one.
    async fn names_one_entry(
        &self,
        from: &RelativePath,
        to: &RelativePath,
    ) -> Result<Option<bool>, FsError> {
        let Some(from) = self.entry_at(from).await?.map(|found| found.spelled) else {
            return Ok(None);
        };
        Ok(Some(
            self.entry_at(to)
                .await?
                .is_some_and(|found| found.spelled == from),
        ))
    }

    async fn remove(&self, path: &RelativePath) -> Result<(), FsError> {
        const ACTION: &str = "removing";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let removed = Box::pin(self.remove_at(&mut changed.batch, path, false)).await;
        changed.send().await;
        written(removed, ACTION, path)
    }

    async fn remove_empty_directory(&self, path: &RelativePath) -> Result<(), FsError> {
        const ACTION: &str = "removing a directory";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let removed = Box::pin(self.remove_empty_directory_at(&mut changed.batch, path)).await;
        changed.send().await;
        if removed == Err(Errno::ENOENT) {
            return Err(FileNotFound {
                resource: path.to_string(),
            }
            .into());
        }
        written(removed, ACTION, path)
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        const ACTION: &str = "removing";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let removed = Box::pin(self.remove_at(&mut changed.batch, path, true)).await;
        changed.send().await;
        written(removed, ACTION, path)
    }

    async fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<FileInfo, FsError> {
        const ACTION: &str = "writing a file";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let wrote = Box::pin(async {
            if let Some(mtime) = self
                .restore_revision_content(&mut changed.batch, &repository, node, path)
                .await?
            {
                return Ok(Some(mtime));
            }
            let (staged, _) = self.stage_stored(repository, node.address, path).await?;
            let executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
            written(
                self.write_content(
                    &mut changed.batch,
                    path,
                    Content::Host(&staged.0),
                    Some(executable),
                )
                .await,
                ACTION,
                path,
            )
            .map(|()| None)
        })
        .await;
        changed.send().await;
        match wrote? {
            Some(mtime) => Ok(FileInfo::from_node_and_mtime(node, mtime)),
            None => InstanceOperation::file_info(self, path).await,
        }
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        const ACTION: &str = "writing a file";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let wrote = Box::pin(async {
            let (staged, fragment) = self.stage_stored(repository, address, path).await?;
            written(
                self.write_content(&mut changed.batch, path, Content::Host(&staged.0), None)
                    .await,
                ACTION,
                path,
            )?;
            Ok((fragment, None))
        })
        .await;
        changed.send().await;
        wrote
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        const ACTION: &str = "copying a file";
        self.writable(ACTION)?;
        let mut changed = self.changes();
        let copied = Box::pin(async {
            use std::os::unix::fs::PermissionsExt as _;

            let staged;
            // The source's mode goes with its bytes, as `std::fs::copy` copies it.
            let (host, executable) = match self.file_source(source_path).await? {
                Some(FileSource::Local(host)) => {
                    let mode = std::fs::metadata(&host)
                        .map_err(|error| host_error(source_path, &error))?
                        .permissions()
                        .mode();
                    (host, mode & 0o111 != 0)
                }
                Some(FileSource::Tracked(entry)) => {
                    let repository = self.filesystem.served().repository.clone();
                    staged = self
                        .stage_stored(repository, entry.address, source_path)
                        .await?
                        .0;
                    let executable =
                        entry.mode & NodeFileMode::Executable == NodeFileMode::Executable;
                    (staged.0.clone(), executable)
                }
                None => return written(Err(Errno::ENOENT), ACTION, source_path),
            };
            written(
                self.write_content(
                    &mut changed.batch,
                    destination_path,
                    Content::Host(&host),
                    Some(executable),
                )
                .await,
                ACTION,
                destination_path,
            )
        })
        .await;
        changed.send().await;
        copied
    }

    fn stored(&self, path: &RelativePath, address: Address, size: u64, mode: u16, mtime: u64) {
        self.served.store(
            path,
            StoredContent {
                address,
                size,
                mode,
            },
            mtime,
        );
    }

    async fn committed(
        &self,
        repository: Arc<RepositoryContext>,
        committed: Arc<State>,
    ) -> Result<(), FsError> {
        // Boxed: the reconciliation walks every file the commit read, once per commit.
        Box::pin(async move {
            // Taken first, so every exit drops the blocks once the settle returns, and leaves
            // the debt owed unless the settle completed with no error.
            let _serialized = self.serialized.take();
            let mut owed = self.owed.take();
            let handed = match owed.as_mut() {
                Some(owed) => std::mem::take(&mut owed.handed),
                None => self.take_handed(),
            };
            if !self.filesystem.write_step_allowed(WriteStep::Reconcile) {
                return Err(FsError::internal("the reconcile was refused"));
            }
            let clean = self
                .filesystem
                .scoped(
                    self.filesystem
                        .committed(repository, committed, &self.served, handed),
                )
                .await
                .forward_any::<FsError>("reconciling the mount with the commit")?;
            if clean && let Some(owed) = owed {
                owed.release();
            }
            Ok(())
        })
        .await
    }

    async fn infer_is_diffable(&self, path: &RelativePath) -> Result<bool, FsError> {
        // Boxed so the 4 KiB buffer lives in one allocation, not in every caller's future.
        Box::pin(async {
            let entry = match self.file_source(path).await? {
                Some(FileSource::Local(host)) => {
                    return Ok(
                        crate::infer::infer_is_diffable(&ContentSource::owned_file(host))
                            .await
                            .unwrap_or(false),
                    );
                }
                Some(FileSource::Tracked(entry)) => entry,
                None => return Ok(false),
            };
            let repository = self.filesystem.served().repository.clone();
            let len =
                usize::try_from(entry.size).map_or(INFER_PREFIX, |size| size.min(INFER_PREFIX));
            let mut buffer = [0u8; INFER_PREFIX];
            let options = crate::immutable::read_options_from_repository(&repository);
            self.filesystem
                .scoped(crate::immutable::read_into(
                    repository,
                    entry.address,
                    Some(0..len),
                    &mut buffer[..len],
                    options,
                ))
                .await
                .forward_any::<FsError>("reading the start of a mounted file")?;
            Ok(crate::infer::infer_is_diffable_by_slice(&buffer[..len]))
        })
        .await
    }
}

/// A directory's entries as the filesystem State holds them, skipping view-excluded ones; a
/// local entry reports what its host file holds.
pub struct FuseDirectoryListing {
    listing: Arc<Listing>,
    index: usize,
    /// Where the directory lives on the host, which local entries are read from.
    host: Option<PathBuf>,
    base_time: u64,
    /// Each file's time by listing index; `None` while no path has a recorded time.
    mtimes: Option<Box<[u64]>>,
}

impl FuseDirectoryListing {
    pub(crate) async fn next(&mut self) -> Option<Result<DirectoryEntry, FsError>> {
        loop {
            // An ordinary clone's listing skips a symlink too (`crate::fs::os::file_list_item`).
            match self.next_with_untracked().await? {
                Ok(ListedEntry::Tracked(entry)) => return Some(Ok(entry)),
                Ok(ListedEntry::Untracked(_)) => {}
                Err(err) => return Some(Err(err)),
            }
        }
    }

    /// The next entry as [`next`](Self::next) answers it, a symbolic link included, by its name.
    pub(crate) async fn next_with_untracked(&mut self) -> Option<Result<ListedEntry, FsError>> {
        while self.index < self.listing.len() {
            let index = self.index;
            self.index += 1;
            let Some(entry) = self.listing.current_entry(index) else {
                continue;
            };
            if entry.flags & entry_flags::VIEW_EXCLUDED != 0 {
                continue;
            }
            let Ok(name) = std::str::from_utf8(self.listing.name(index)) else {
                continue;
            };
            if entry.flags & entry_flags::SYMLINK != 0 {
                return Some(Ok(ListedEntry::Untracked(name.to_string())));
            }
            let info = if entry.flags & entry_flags::LOCAL != 0 && !is_dir(&entry) {
                let Some(host) = &self.host else {
                    continue;
                };
                let at = host.join(name);
                match host_info(&at).await {
                    Ok(info) if info.exists() => info,
                    Ok(_) => continue,
                    Err(error) => return Some(Err(host_error(at.display(), &error))),
                }
            } else {
                let mtime = self
                    .mtimes
                    .as_ref()
                    .map_or(self.base_time, |mtimes| mtimes[index]);
                file_info_of(&entry, mtime)
            };
            return Some(Ok(ListedEntry::Tracked(DirectoryEntry {
                name: name.to_string(),
                info,
                name_hash: entry.name_hash,
            })));
        }
        None
    }
}
