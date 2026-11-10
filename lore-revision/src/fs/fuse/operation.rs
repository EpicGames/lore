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
use std::sync::atomic::Ordering;

use bytes::Bytes;
use crossbeam_epoch as epoch;
use crossbeam_epoch::Atomic;
use crossbeam_epoch::Owned;
use crossbeam_epoch::Shared;
use crossbeam_queue::SegQueue;
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

use crate::errors::NotSupported;
use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::DirectoryListing;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::StaticDispatchDirectoryListing;
use crate::fs::fuse::filesystem::CommitReading;
use crate::fs::fuse::filesystem::FuseFilesystem;
use crate::fs::fuse::filesystem::LookupCase;
use crate::fs::fuse::filesystem::SettleOwed;
use crate::fs::fuse::filesystem::SpelledEntry;
use crate::fs::fuse::filesystem::WriteStep;
use crate::fs::fuse::inode::Name;
use crate::fs::fuse::inode::ROOT_INO;
use crate::fs::fuse::listing::Listing;
use crate::fs::fuse::listing::ListingEntry;
use crate::fs::fuse::listing::entry_flags;
use crate::fs::fuse::local_tree::LocalKind;
use crate::fs::fuse::notifications::Invalidations;
use crate::fs::fuse::notifications::Pending;
use crate::fs::fuse::served::Served;
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
use crate::state::SerializedBlocks;
use crate::state::State;
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
    /// The directories the commit adds or moves, as its walk's parallel tasks hand them over
    /// ([`Self::hand_directory`]); made by the first, as most operations hand none and the queue
    /// is two cache lines.
    handed: std::sync::OnceLock<Box<SegQueue<RelativePath>>>,
    /// The committed State's blocks as its serialize wrote them, held until the settle against
    /// that State returns, so the settle reads them from memory.
    serialized: AtomicCell<Option<Box<SerializedBlocks>>>,
    /// The settle the commit owes, from before its revision may become current
    /// ([`Self::owe_settle`]).
    owed: AtomicCell<Option<Box<SettleOwed>>>,
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
/// the content, what it stored.
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

    /// Records that the content served for `path` was stored as `stored`.
    pub(crate) fn store(&self, path: &RelativePath, stored: StoredContent) {
        if let Some(mut file) = self.0.get_mut(path.as_str()) {
            file.stored = Some(stored);
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

impl FuseOperation {
    pub fn new(filesystem: Arc<FuseFilesystem>) -> Self {
        Self {
            filesystem,
            served: ServedContent::default(),
            reading: std::sync::OnceLock::new(),
            revision: tokio::sync::OnceCell::new(),
            deletions_read: ListedDeletions::default(),
            handed: std::sync::OnceLock::new(),
            serialized: AtomicCell::new(None),
            owed: AtomicCell::new(None),
        }
    }

    pub fn filesystem(&self) -> &Arc<FuseFilesystem> {
        &self.filesystem
    }

    /// Collects `path`, a directory the commit adds or moves, for its settle.
    pub(crate) fn hand_directory(&self, path: &RelativePath) {
        self.handed
            .get_or_init(|| Box::new(SegQueue::new()))
            .push(path.clone());
    }

    /// The directories handed over so far, taken.
    fn take_handed(&self) -> Vec<RelativePath> {
        self.handed
            .get()
            .map(|handed| std::iter::from_fn(|| handed.pop()).collect())
            .unwrap_or_default()
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
/// own is ([`host_file_info`]).
async fn host_info(host: &Path) -> std::io::Result<FileInfo> {
    match lore_io::IoDriver::global().metadata(host).await {
        Ok(metadata) => Ok(host_file_info(&metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileInfo::NotExist),
        Err(error) => Err(error),
    }
}

/// [`FileInfo::from_metadata`], a host file's time the later of its modification and its status
/// change, in milliseconds. A file renamed onto a name, created at it, or given a time carries
/// that change's time, so it never matches a time recorded for the file the name held before,
/// whatever its own modification time. Commits and scans record and compare this time; no
/// served attribute reads it ([`crate::fs::fuse::attr::RecordedTimes`]).
fn host_file_info(metadata: &std::fs::Metadata) -> FileInfo {
    use std::os::unix::fs::MetadataExt as _;

    match FileInfo::from_metadata(metadata) {
        FileInfo::File {
            executable,
            size,
            mtime,
        } => {
            let changed = u64::try_from(metadata.ctime())
                .unwrap_or_default()
                .saturating_mul(1000)
                .saturating_add(
                    u64::try_from(metadata.ctime_nsec()).unwrap_or_default() / 1_000_000,
                );
            FileInfo::File {
                executable,
                size,
                mtime: mtime.max(changed),
            }
        }
        info => info,
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

/// `result` as an instance write reports it: a read-only provider's refusal as [`NotSupported`].
fn written<T>(result: Result<T, Errno>, action: &str, path: &RelativePath) -> Result<T, FsError> {
    match result {
        Ok(done) => Ok(done),
        Err(Errno::EROFS) => read_only(action),
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
    /// the list records, which only its local copy survives at the next mount. Answers whether it
    /// did.
    async fn restore_revision_content(
        &self,
        changed: &mut Invalidations,
        node: &Node,
        path: &RelativePath,
    ) -> Result<bool, FsError> {
        let Some(located) = self.located(path).await? else {
            return Ok(false);
        };
        if is_dir(&located.entry) {
            return Ok(false);
        }
        let served = self.filesystem.served();
        let revision = self
            .revision
            .get_or_try_init(|| {
                crate::fs::fuse::filesystem::load_revision(&served.repository, served.revision())
            })
            .await
            .forward_any::<FsError>("loading the mounted revision")?;
        if revision.revision() != served.revision() {
            return Ok(false);
        }
        let held = match revision
            .find_relative_node_link(
                served.repository.clone(),
                ROOT_NODE,
                located.spelled.as_str(),
            )
            .await
        {
            Ok(link) if link.revision == revision.revision() && link.node == located.node => {
                revision
                    .node(served.repository.clone(), link.node)
                    .await
                    .forward_any::<FsError>("reading the mounted revision")?
            }
            _ => return Ok(false),
        };
        if !held.is_file()
            || held.address.hash != node.address.hash
            || held.size != node.size
            || held.mode != node.mode
            || self.deletion_listed(&located.spelled)
        {
            return Ok(false);
        }
        let target = StoredContent {
            address: held.address,
            size: held.size,
            mode: held.mode,
        };
        let restored = self
            .filesystem
            .overwrite_local_file(
                &served,
                &located.spelled,
                located.node,
                (located.dir, located.entry.name_hash),
                &target,
            )
            .await
            .forward_any_with::<FsError, _>(|| format!("restoring '{path}' to the revision"))?;
        if restored && let Some(ino) = self.filesystem.live_inode(&located.spelled) {
            changed.inodes.push(ino);
        }
        Ok(restored)
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

impl InstanceOperation for FuseOperation {
    /// Diffs the filesystem State at `diff.filesystem_path` against `diff.from` by address,
    /// descending only where either side is dirty, and marks `diff.from` only as the intent asks.
    /// The State is read under the filter of `diff.from`'s repository, the command's, as an
    /// ordinary walk lists the working tree under it: the ignore rules a command reads are its
    /// own, where the provider's context holds those of the last revision it held.
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        let filesystem = self.filesystem.clone();
        ChangeStream::spawn(async move |changes| {
            let served = filesystem.served();
            let node = served
                .state
                .find_node_link(served.repository.clone(), diff.filesystem_path.as_str())
                .await
                .map_or(INVALID_NODE, |link| link.node);
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
            // Boxed: the walk and its settling make a large future, one per diff.
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

    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        if path.is_empty() {
            return Ok(FileInfo::Directory);
        }
        let Some(located) = self.located(path).await? else {
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

    async fn untracked_file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        Ok(self.local_info(path).await?.unwrap_or(FileInfo::NotExist))
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
        let next = self.filesystem.inodes().next_incarnation();
        let held = self.filesystem.live_inode(path).and_then(|ino| {
            self.filesystem
                .inode_incarnation(ino)
                .map(|(incarnation, generation)| (ino, incarnation, generation))
        });
        let Some(located) = self
            .located(path)
            .await?
            .filter(|located| !is_dir(&located.entry))
        else {
            return Err(FileNotFound {
                resource: path.to_string(),
            }
            .into());
        };
        let (ino, incarnation, generation) = match held {
            Some((ino, incarnation, generation)) => (Some(ino), incarnation, generation),
            None => (None, next, 0),
        };
        self.served.record(
            path,
            ServedFile {
                spelled: located.spelled,
                node: located.node,
                place: (located.dir, located.entry.name_hash),
                ino,
                incarnation,
                generation,
                stored: None,
            },
        );
        Ok(match located.host {
            Some(host) => ContentSource::owned_file(host),
            None => ContentSource::virtual_immutable(located.entry.address, located.entry.size),
        })
    }

    /// A local file is measured against the stored object as a real file is; a tracked file
    /// holds its State address.
    async fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<NodeComparison, FsError> {
        Ok(match self.file_source(path).await? {
            Some(FileSource::Local(host)) => {
                let matched = crate::immutable::file_matches(
                    repository,
                    previous,
                    Some(previous_size as usize),
                    &ContentSource::owned_file(host),
                    established,
                )
                .await
                .forward_any::<FsError>("comparing a local file to stored content")?;
                crate::state::node_comparison(matched)
            }
            Some(FileSource::Tracked(entry))
                if entry.address.hash == previous.hash && entry.size == previous_size =>
            {
                NodeComparison::Matches
            }
            _ => NodeComparison::Differs,
        })
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
            if self
                .restore_revision_content(&mut changed.batch, node, path)
                .await?
            {
                return Ok(());
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
        })
        .await;
        changed.send().await;
        wrote?;
        InstanceOperation::file_info(self, path).await
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

    fn stored(&self, path: &RelativePath, address: Address, size: u64, mode: u16) {
        self.served.store(
            path,
            StoredContent {
                address,
                size,
                mode,
            },
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
        while self.index < self.listing.len() {
            let index = self.index;
            self.index += 1;
            let Some(entry) = self.listing.current_entry(index) else {
                continue;
            };
            // An ordinary clone's listing skips a symlink too (`crate::fs::os::file_list_item`).
            if entry.flags & (entry_flags::VIEW_EXCLUDED | entry_flags::SYMLINK) != 0 {
                continue;
            }
            let Ok(name) = std::str::from_utf8(self.listing.name(index)) else {
                continue;
            };
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
            return Some(Ok(DirectoryEntry {
                name: name.to_string(),
                info,
                name_hash: entry.name_hash,
            }));
        }
        None
    }
}
