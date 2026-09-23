// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Core filesystem provider traits for repository operations.
//!
//! This module defines the two-trait architecture that separates operation context creation
//! (freeze for SWFS) from actual file operations (work against frozen snapshot).

use std::fs::Metadata;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::InvalidArguments;
use lore_base::types::Fragment;
use lore_error_set::ErrorSet;
use lore_error_set::error_set;
use lore_error_set::prelude::*;

use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::os::OsDirectoryListing;
use crate::fs::os::OsOperation;
use crate::fs::swfs::filesystem::SwfsOperation;
use crate::hash::hash_string;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore_trace;
use crate::node::Node;
use crate::node::NodeFileMode;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::LayerMountInfo;
use crate::state::LinkMountInfo;
use crate::state::NodeComparison;
use crate::state::NodeMapping;
use crate::state::RecordedModifiedTimes;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum FsError {
    InvalidArguments,
}

impl From<std::io::Error> for FsError {
    fn from(value: std::io::Error) -> Self {
        FsError::internal(value.to_string())
    }
}

/// What a walk measured at a path, as returned by `InstanceOperation::file_info`.
///
/// The repository tracks files and directories. Anything else the file system holds — a device, a
/// socket — answers as holding nothing, and a directory listing skips a link rather than
/// describing what it points at. A path named on its own is stat'd through a link, so one naming
/// a link answers for the target it resolves to. Supporting links means giving them a variant of
/// their own rather than widening one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileInfo {
    /// The path holds nothing, which a path the walk skipped also answers.
    NotExist,
    /// A directory, which stores none of the size, time or mode a file does.
    Directory,
    /// A file, with what the walk measured of it.
    File {
        /// Whether the file carries the executable bit, `None` where the platform has no
        /// such bit to read. See [`FileInfo::mode`].
        executable: Option<bool>,
        /// File size in bytes.
        size: u64,
        /// Modification time as Unix timestamp in milliseconds.
        mtime: u64,
    },
}

impl FileInfo {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        if metadata.is_dir() {
            return FileInfo::Directory;
        }
        if !metadata.is_file() {
            return FileInfo::NotExist;
        }
        let (mtime, size) = crate::util::fs::file_mtime_and_size(metadata);
        FileInfo::File {
            executable: crate::util::fs::file_executable_observed(metadata),
            size,
            mtime,
        }
    }

    pub fn from_node_and_mtime(node: &Node, mtime: u64) -> Self {
        FileInfo::File {
            executable: Some(node.mode & NodeFileMode::Executable == NodeFileMode::Executable),
            size: node.size,
            mtime,
        }
    }

    /// Whether the file system holds anything here that the repository tracks.
    pub fn exists(&self) -> bool {
        !matches!(self, FileInfo::NotExist)
    }

    pub fn is_file(&self) -> bool {
        matches!(self, FileInfo::File { .. })
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, FileInfo::Directory)
    }

    /// The size of a file, and zero for anything else, which stores none.
    pub fn size(&self) -> u64 {
        match self {
            FileInfo::File { size, .. } => *size,
            _ => 0,
        }
    }

    /// The modification time of a file, and zero for anything else, which stores none.
    pub fn mtime(&self) -> u64 {
        match self {
            FileInfo::File { mtime, .. } => *mtime,
            _ => 0,
        }
    }

    /// Whether a file carries the executable bit, `None` where the platform has no such bit to
    /// read and where the path holds no file to read it from.
    pub fn executable(&self) -> Option<bool> {
        match self {
            FileInfo::File { executable, .. } => *executable,
            _ => None,
        }
    }

    /// The mode to store on a node whose mode is `previous`, as
    /// [`crate::util::fs::mode_from_observed`] answers it for what was read off the
    /// metadata this describes.
    pub fn mode(&self, previous: u16) -> u16 {
        crate::util::fs::mode_from_observed(self.is_file(), self.executable(), previous)
    }

    /// Whether this carries a different mode from `previous`, which is a modification of the
    /// file in its own right.
    ///
    /// A platform with no executable bit to read answers no, since `previous` is then what
    /// [`Self::mode`] stores.
    pub fn mode_differs_from(&self, previous: u16) -> bool {
        crate::util::fs::mode_changed(previous, self.mode(previous))
    }
}

/// One child of a directory listing.
pub struct DirectoryEntry {
    /// The child's name within its directory, not a path.
    pub name: String,
    /// What the listing measured at the name, which is a file or a directory: a listing yields
    /// no entry for what the repository holds nothing for.
    pub info: FileInfo,
    /// The hash of the lowercase name, which is what claiming the child among a node's children
    /// takes. Taken from the name this entry carries.
    pub name_hash: u64,
}

/// Wraps every type a provider lists a directory with, so a listing dispatches statically for
/// the same reason [`InstanceOperationImpl`] does.
pub enum StaticDispatchDirectoryListing {
    Os(OsDirectoryListing),
}

/// Directory entries, resolved as the consumer asks for them.
pub struct DirectoryListing {
    dispatch: StaticDispatchDirectoryListing,
}

impl DirectoryListing {
    pub fn new(dispatch: StaticDispatchDirectoryListing) -> Self {
        Self { dispatch }
    }

    /// The next entry, or `None` once the directory is exhausted.
    ///
    /// Reads no further than the entry asked for, so a consumer that stops early stops the walk
    /// with it.
    pub async fn next(&mut self) -> Option<Result<DirectoryEntry, FsError>> {
        match &mut self.dispatch {
            StaticDispatchDirectoryListing::Os(this) => this.next().await,
        }
    }
}

/// A tree to diff against, before a path in it is resolved to a root. Resolving one
/// yields the [`NodeMapping`] the diff walks.
pub struct FilesystemDiffTree {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
}

/// What a staging walk records beyond the dirty flags every marking walk sets.
#[derive(Debug, Clone, Copy, Default)]
pub struct StageIntent {
    /// Set on every node the walk stages, beyond the staged action itself.
    pub node_flags: NodeFlags,
    /// The identity a new file node takes. A new one is minted where absent, so
    /// metadata can be attached before a commit assigns one.
    ///
    /// One identity serves the whole walk, so a caller supplying it stages one file.
    /// A move is paired to its delete by identity, and repeating one across unrelated
    /// files pairs those instead.
    pub file_id: Option<Context>,
}

/// What a filesystem diff does with the differences it finds.
#[derive(Debug, Clone, Copy)]
pub enum FilesystemDiffIntent {
    /// Report them, leaving the trees untouched.
    Report,
    /// Set and clear `Dirty` on each node as the walk settles it.
    MarkDirty,
    /// Mark as [`MarkDirty`](Self::MarkDirty) does and record the staged action too.
    Stage(StageIntent),
}

impl FilesystemDiffIntent {
    /// Whether the walk persists what it finds as dirty flags rather than only reporting.
    pub fn marks_dirty(self) -> bool {
        matches!(
            self,
            FilesystemDiffIntent::MarkDirty | FilesystemDiffIntent::Stage(_)
        )
    }

    /// What the walk stages beyond marking, where it stages at all.
    pub fn stage(self) -> Option<StageIntent> {
        match self {
            FilesystemDiffIntent::Stage(intent) => Some(intent),
            _ => None,
        }
    }
}

/// What to diff against the filesystem: `from` is the tree it is compared against and
/// `current` is what the working copy last held, which is how an unstaged add is told
/// apart from a tracked file.
pub struct FilesystemDiffContext {
    /// The operation the walk reads the working tree through, carried here because the walk
    /// spawns tasks and so needs one it can own rather than borrow.
    pub operation: Arc<InstanceOperationImpl>,
    pub from: NodeMapping,
    pub current: NodeMapping,
    /// The path as the file system spells it, which parts from the mappings' own spelling only
    /// where the two name a component with different case.
    pub filesystem_path: RelativePath,
    /// The filter's verdict at `filesystem_path`, which each child steps from rather
    /// than refolding the ancestors it already accounts for.
    pub states: FilterStates,
    /// The same verdict on the `from` side, which diverges from `states` once a move
    /// puts the two sides at different paths.
    pub from_states: FilterStates,
    pub filter_mode: FilterMode,
    pub intent: FilesystemDiffIntent,
    pub layer_mounts: Arc<Vec<LayerMountInfo>>,
    /// Every link mount in the compared tree, so a mount is told from a directory only
    /// the filesystem holds. Read from the trees, which is why the caller supplies it.
    pub link_mounts: Arc<Vec<LinkMountInfo>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileDifferenceFromNode {
    /// Whether the file content differs from the node.
    pub modified: bool,
    /// Whether the file carries a different mode from the node, which is a modification of the
    /// file in its own right and one the content answers nothing about.
    pub mode_differs: bool,
}

/// The node a file was measured against, and whether the current revision is what holds it.
#[derive(Debug, Clone, Copy)]
pub struct MeasuredNode {
    /// The node the file was measured against.
    pub node: Node,
    /// Whether the current revision is what holds it.
    pub is_current: bool,
}

/// Result of checking whether a file differs from a node.
#[derive(Debug, Clone, Copy)]
pub struct FileModifiedCheck {
    /// Basic file information.
    pub info: FileInfo,
    /// The node the file was measured against, where the state held one.
    pub measured: Option<MeasuredNode>,
    /// If it made sense for the difference to be computed (a file exists on the file system and the
    /// Merkle tree State had a node that was a file and not a directory).
    pub modification: Option<FileDifferenceFromNode>,
}

/// Filesystem provider trait - creates operation contexts.
///
/// For OS-backed filesystems, this is a simple factory.
/// For SWFS, this is where the filesystem freeze occurs.
#[async_trait]
pub trait FilesystemProvider: Send + Sync + 'static {
    /// Create a new filesystem operation context.
    ///
    /// A filesystem holds one operation at a time and the next begins once that one is
    /// finalized. The provider covers a whole mounted filesystem, so an operation covers
    /// every repository mounted in it: a link or layer at a subpath is a subtree of the
    /// same filesystem and takes the operation its parent already holds.
    ///
    /// # Implementation notes
    ///
    /// - **`OsFilesystem`**: Returns a lightweight wrapper with no state.
    /// - **`SWFS`**: Freezes the filesystem, creates a snapshot, returns operations that work
    ///   against the snapshot.
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError>;
}

/// Runs `work` inside one filesystem operation, finalizing it whether or not the work
/// succeeded so a failure never leaves a filesystem frozen.
///
/// The finalize is told what the work wrote, which the operation records as each write is
/// asked for. The work's error is reported ahead of a finalize failure, being the one that
/// explains the run.
pub async fn with_operation<T, E, F>(
    filesystem: Arc<dyn FilesystemProvider>,
    work: F,
) -> Result<T, E>
where
    E: ErrorSet,
    F: AsyncFnOnce(Arc<InstanceOperationImpl>) -> Result<T, E>,
{
    let operation = filesystem
        .begin_operation()
        .await
        .forward_any::<E>("Failed to start filesystem operation")?;
    let result = work(operation.clone()).await;
    let finalized = operation
        .finalize()
        .await
        .forward_any::<E>("Failed to finish filesystem operation");
    let value = result?;
    finalized?;
    Ok(value)
}

/// [`with_operation`] for work that reads the filesystem only under some conditions.
///
/// Opening an operation freezes a virtual filesystem and snapshots it, so work that reads the
/// working tree only when asked to takes one only then, and the same one for the whole of it.
/// `work` is handed nothing where none was opened, which is the instruction not to read.
pub async fn with_operation_if<T, E, F>(
    filesystem: Arc<dyn FilesystemProvider>,
    needed: bool,
    work: F,
) -> Result<T, E>
where
    E: ErrorSet,
    F: AsyncFnOnce(Option<Arc<InstanceOperationImpl>>) -> Result<T, E>,
{
    if !needed {
        return work(None).await;
    }

    with_operation(filesystem, async |operation| work(Some(operation)).await).await
}

/// Creates `path` and any missing ancestor, for a directory no file written below it creates on
/// the way: one the revision holds with nothing in it, or one whose every child the view filter
/// leaves out.
///
/// A directory already standing there is the state asked for. Anything else standing there is
/// reported, the caller holding a directory and nothing to put in that thing's place.
pub async fn create_empty_directory<E>(
    operation: &InstanceOperationImpl,
    path: &RelativePath,
) -> Result<(), E>
where
    E: ErrorSet,
{
    operation
        .create_dir_all(path)
        .await
        .forward_any_with::<E, _>(|| format!("Failed to create directory {path}"))?;
    lore_trace!("Created empty directory: {path}");
    Ok(())
}

/// Instance operation trait - performs file operations within a context.
///
/// Operations are performed against a consistent snapshot (for SWFS) or directly
/// against the filesystem (for OS-backed).
///
/// This type is not dyn-safe, async methods don't have their future boxed to allow static dispatch
/// though an `impl InstanceOperation`
pub trait InstanceOperation: Send + Sync {
    /// Diff the filesystem under `diff.filesystem_path` against the trees it names, answering
    /// with the changes as the walk finds them.
    ///
    /// Reports files added, modified or deleted on disk, and metadata changes.
    /// `diff.intent` decides whether the trees are marked as it goes.
    ///
    /// Returns rather than walks: the walk runs behind the stream, bounded in how far ahead of
    /// its reader it may get. What the caller does with the changes -- collect them, read each
    /// once, or stop at the first of some kind -- is the stream's to answer and not this.
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats>;

    /// Get basic file information for a path.
    ///
    /// Returns file existence, type, size, mtime, and mode without checking
    /// content modification against a node.
    fn file_info(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send;

    /// What [`file_info`](Self::file_info) reports, read from the working tree rather than
    /// from the tree the path is tracked in.
    ///
    /// For a path known to lie outside the tracked revision tree and to exist in the
    /// filesystem alone — a merge sidecar, a scratch file. A provider serving tracked content
    /// virtually holds no node for one, so it cannot answer for it from what it tracks, and
    /// [`file_info`](Self::file_info) reports it absent. A caller asking about a path the
    /// revision does track asks there instead.
    fn untracked_file_info(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send;

    /// Whether the directory holding `path` holds a child named exactly as `path` spells it.
    ///
    /// The question a case resolution nearly always has, and one lookup answers it: no
    /// directory read and no string to hold the answer. `Some(false)` says the name is not
    /// held in this spelling rather than that it is absent, and `None` is the filesystem
    /// declining to say — macOS for every name, Windows past its path limit — which only
    /// [`names_folding_to`](Self::names_folding_to) answers.
    fn holds_name_exactly(&self, path: &RelativePath) -> impl Future<Output = Option<bool>> + Send;

    /// Every spelling of `name` the directory at `path` holds that folds to the same name, and
    /// empty where no spelling is.
    ///
    /// Reads the directory, which is what answering for a spelling other than the one asked
    /// about takes. A caller that only needs to know whether its own spelling is the one on
    /// disk asks [`holds_name_exactly`](Self::holds_name_exactly).
    ///
    /// Names are claimed by the fold [`read_directory`](Self::read_directory) already carries,
    /// which is the same digest a node's children are matched on: a spelling this reports is one
    /// the tree would resolve to the same node. A link is not among them, a listing holding no
    /// entry for one.
    ///
    /// Derived from [`read_directory`](Self::read_directory). A provider that can answer without
    /// reading the directory overrides this.
    fn names_folding_to(
        &self,
        path: &RelativePath,
        name: &str,
    ) -> impl Future<Output = Result<Vec<String>, FsError>> + Send {
        async move {
            let folded = hash_string(name);
            let mut listing = self.read_directory(path).await?;
            let mut matches = Vec::new();
            while let Some(entry) = listing.next().await {
                let entry = entry?;
                if entry.name_hash == folded {
                    matches.push(entry.name);
                }
            }
            Ok(matches)
        }
    }

    /// The children of the directory at `path`, described as the repository tracks them.
    ///
    /// A link, a device and anything else the repository holds nothing for is left out, as is a
    /// name that could not be read: one unreadable name says nothing about the rest of the
    /// directory. A name that is not text is reported, nothing being possible with such a name
    /// that is not a guess.
    ///
    /// Returns rather than reads: the listing resolves entries as its consumer asks for them, so
    /// a directory of any width costs the same to hold and a consumer that stops early stops the
    /// walk with it. A path holding no directory is reported here rather than as a listing of
    /// nothing.
    fn read_directory(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<DirectoryListing, FsError>> + Send;

    /// Where the content at `path` is read from, in this operation's view of the working tree.
    ///
    /// The provider's own business is where content is held, so it answers with a source rather
    /// than with content, a hash or a comparison: nothing here commits it to reading. Whether a
    /// file holds content already stored is [`file_holds_content`](Self::file_holds_content),
    /// which a provider keeping its own record answers without reading at all.
    ///
    /// A caller that needs the bytes — to address content matching nothing stored — takes its
    /// source from here rather than naming a host path the provider may not read through.
    fn content_source(&self, path: &RelativePath) -> lore_storage::ContentSource<'static>;

    /// Whether the file at `path` holds the content `previous` addresses, and `previous_size`
    /// bytes of it.
    ///
    /// How the answer is reached is the operation's own business. One materializing its files
    /// reads and measures them; one that records what it wrote answers from that record without
    /// reading anything. A file that cannot be read is reported as such rather than as either
    /// answer, so a caller does not act on a comparison that never happened.
    ///
    /// Takes the address to compare against rather than a change, so a caller holding both sides
    /// of one can ask about either. Reads no recorded modification time: a recorded time speaks
    /// for the node the current revision holds and answers for no other.
    ///
    /// `established` carries what comparing this file has already settled, so a caller measuring
    /// one path against several addresses reads it no more than the answers require. A provider
    /// answering without reading leaves it untouched.
    fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> impl Future<Output = Result<NodeComparison, FsError>> + Send;

    /// Make a file executable (Unix) or set executable bit equivalent.
    ///
    /// On Windows, this is a no-op.
    fn make_executable(
        &self,
        path: &RelativePath,
        executable: bool,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Create a directory if it doesn't exist (mkdir -p behavior).
    fn create_dir_all(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Writes `contents` to `path` in the file system the operation was opened on, replacing what
    /// is there. Empty contents leave an empty file.
    ///
    /// Raw bytes the caller holds, written as they are: the merged text a text conflict resolves
    /// to, and the empty base a merge leaves beside a file where the revision holds none. The
    /// path may well be one the revision tracks; the bytes are not, coming from the caller rather
    /// than from a node.
    ///
    /// Content a node addresses is written by [`write_node`](Self::write_node) or
    /// [`set_file_to_immutable_store_contents`](Self::set_file_to_immutable_store_contents),
    /// which take it from the immutable store rather than from the caller.
    fn write_file(
        &self,
        path: &RelativePath,
        contents: Bytes,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Moves what the file system holds at `from` to `to`, unifying the two where `to` is
    /// occupied rather than refusing: a file takes the place of the file there, and a directory
    /// hands each of its children over under these same rules before it goes.
    ///
    /// A `from` and `to` of different kinds, one a file and one a directory, is refused, there
    /// being no move that leaves one of them.
    ///
    /// A case change is a move like any other, `from` and `to` differing only in spelling. It is
    /// what the unification is for: a file system holding both spellings side by side leaves two
    /// entries to merge rather than one to rename.
    fn rename(
        &self,
        from: &RelativePath,
        to: &RelativePath,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Delete a file or empty directory.
    fn remove(&self, path: &RelativePath) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Delete a directory and all contents.
    fn remove_recursive(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Write `node`'s content to `path`, creating the directory above it, applying the node's
    /// mode, and reporting what the file looks like once written.
    ///
    /// One call rather than four because the steps are never useful apart, and each one asked
    /// separately resolves the same name against the root again.
    ///
    /// A path that cannot be read back once written is an error: what the file looks like is
    /// the point of the call, and a caller records a modified time from it.
    fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send;

    /// Sets the file at `path` to the content `node` addresses, reporting the fragment it came
    /// from and what the file looks like where the write reported it.
    ///
    /// A node of no size addresses no stored content and leaves an empty file, the store holding
    /// nothing to read it from. A caller materializing a tree asks here for every node and does
    /// not sort the empty ones out itself.
    ///
    /// The file information is `None` where the write did not report it, which a caller needing
    /// it answers with [`file_info`](Self::file_info).
    fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> impl Future<Output = Result<(Fragment, Option<FileInfo>), FsError>> + Send;

    /// Copy the contents of `source_path` to `destination_path`, with the destination being a
    /// scratch file that is not expected to be part of the repository even if it's in its path.
    fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Whether the content at `path` can be diffed, or must only be compared opaquely.
    ///
    /// Reads the head of the content, which is what telling text from an opaque format takes.
    /// Content that cannot be read is not diffable, there being nothing to diff.
    ///
    /// Derived from [`content_source`](Self::content_source). A provider that answers without
    /// reading overrides this.
    fn infer_is_diffable(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<bool, FsError>> + Send {
        async move {
            Ok(crate::infer::infer_is_diffable(&self.content_source(path))
                .await
                .unwrap_or(false))
        }
    }
}

/// Implements `InstanceOperation` by wrapping all other types implementing it and forwarding method
/// calls. This type can then be called into to statically dispatch `InstanceOperation` functions
/// while still not knowing which type is in use at compile time.
pub enum StaticDispatchInstanceOperation {
    Os(OsOperation),
    Swfs(SwfsOperation),
    #[cfg(test)]
    Test(tests::TestOperation),
}

pub struct InstanceOperationImpl {
    dispatch: StaticDispatchInstanceOperation,
    finalized: AtomicBool,
    changed: AtomicBool,
    modified_times: RecordedModifiedTimes,
}

impl InstanceOperationImpl {
    pub fn new(dispatch: StaticDispatchInstanceOperation) -> Self {
        Self {
            dispatch,
            finalized: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            modified_times: RecordedModifiedTimes::default(),
        }
    }

    /// Collects that `path` holds the content of the node written there, for a caller that
    /// knows which revision the operation leaves current.
    pub fn record_modified_time(
        &self,
        repository: &RepositoryContext,
        path: &RelativePath,
        mtime: u64,
    ) {
        self.modified_times.record(repository, path, mtime);
    }

    /// Takes the times collected so far. Times left behind are dropped with the operation,
    /// which is what an operation that does not know its resulting revision wants.
    pub fn take_modified_times(&self) -> RecordedModifiedTimes {
        self.modified_times.take()
    }

    /// Whether this call is the one that finalizes, so a second is refused rather than
    /// thawing a filesystem another caller still holds.
    fn claim_finalize(&self) -> bool {
        !self.finalized.swap(true, Ordering::AcqRel)
    }

    /// Collects that the operation was asked to write, which is what [`Self::finalize`] reports.
    ///
    /// Recorded for the call rather than for its outcome: a write that failed part of the way
    /// through leaves the same stale cache behind as one that succeeded.
    fn record_change(&self) {
        self.changed.store(true, Ordering::Release);
    }

    /// Finishes the operation, reporting to the provider behind it whether the work wrote.
    ///
    /// # Implementation notes
    ///
    /// - **`OsOperation`**: Nothing to finish, the writes having gone to the filesystem the
    ///   reads come from.
    /// - **`SWFS`**: Thaws the filesystem, clearing the write cache where writes were made.
    pub async fn finalize(&self) -> Result<(), FsError> {
        if !self.claim_finalize() {
            return Err(FsError::internal("Operation already finalized"));
        }
        let changes_made = self.changed.load(Ordering::Acquire);
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.finalize(changes_made),
            StaticDispatchInstanceOperation::Os(_this) => Ok(()),
            StaticDispatchInstanceOperation::Swfs(this) => this.finalize(changes_made).await,
        }
    }
}

impl InstanceOperation for InstanceOperationImpl {
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => {
                this.changes_from_filesystem_to_state(diff)
            }
            StaticDispatchInstanceOperation::Os(this) => {
                this.changes_from_filesystem_to_state(diff)
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.changes_from_filesystem_to_state(diff)
            }
        }
    }

    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.file_info(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.file_info(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.file_info(path).await,
        }
    }

    async fn untracked_file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.untracked_file_info(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.untracked_file_info(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.untracked_file_info(path).await,
        }
    }

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.holds_name_exactly(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.holds_name_exactly(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.holds_name_exactly(path).await,
        }
    }

    async fn names_folding_to(
        &self,
        path: &RelativePath,
        name: &str,
    ) -> Result<Vec<String>, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.names_folding_to(path, name).await,
            StaticDispatchInstanceOperation::Os(this) => this.names_folding_to(path, name).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.names_folding_to(path, name).await,
        }
    }

    async fn read_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.read_directory(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.read_directory(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.read_directory(path).await,
        }
    }

    fn content_source(&self, path: &RelativePath) -> lore_storage::ContentSource<'static> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.content_source(path),
            StaticDispatchInstanceOperation::Swfs(this) => this.content_source(path),
        }
    }

    async fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<NodeComparison, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.file_holds_content(repository, path, previous, previous_size, established)
                    .await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.file_holds_content(repository, path, previous, previous_size, established)
                    .await
            }
        }
    }

    async fn make_executable(&self, path: &RelativePath, executable: bool) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.make_executable(path, executable).await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.make_executable(path, executable).await
            }
        }
    }

    async fn create_dir_all(&self, path: &RelativePath) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.create_dir_all(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.create_dir_all(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.create_dir_all(path).await,
        }
    }

    async fn write_file(&self, path: &RelativePath, contents: Bytes) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.write_file(path, contents).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.write_file(path, contents).await,
        }
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.rename(from, to).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.rename(from, to).await,
        }
    }

    async fn remove(&self, path: &RelativePath) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.remove(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.remove(path).await,
        }
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.remove_recursive(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.remove_recursive(path).await,
        }
    }

    async fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<FileInfo, FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.write_node(repository, node, path).await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.write_node(repository, node, path).await
            }
        }
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.set_file_to_immutable_store_contents(repository, node, path)
                    .await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.set_file_to_immutable_store_contents(repository, node, path)
                    .await
            }
        }
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        self.record_change();
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.copy_file(source_path, destination_path).await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.copy_file(source_path, destination_path).await
            }
        }
    }

    async fn infer_is_diffable(&self, path: &RelativePath) -> Result<bool, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.infer_is_diffable(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.infer_is_diffable(path).await,
        }
    }
}

#[cfg(test)]
// A fixture builds filesystem state directly, outside any repository; what these
// test is how the provider reads it.
#[allow(clippy::disallowed_methods)]
pub mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use bytes::Bytes;
    use lore_base::types::Fragment;
    use parking_lot::Mutex;

    use crate::fs::filesystem_provider::DirectoryEntry;
    use crate::fs::filesystem_provider::DirectoryListing;
    use crate::fs::filesystem_provider::FileInfo;
    use crate::fs::filesystem_provider::FilesystemDiffContext;
    use crate::fs::filesystem_provider::FilesystemDiffIntent;
    use crate::fs::filesystem_provider::FilesystemDiffTree;
    use crate::fs::filesystem_provider::FilesystemProvider;
    use crate::fs::filesystem_provider::FsError;
    use crate::fs::filesystem_provider::InstanceOperation;
    use crate::fs::filesystem_provider::InstanceOperationImpl;
    use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
    use crate::fs::filesystem_provider::create_empty_directory;
    use crate::fs::filesystem_provider::with_operation;
    use crate::fs::filesystem_provider::with_operation_if;
    use crate::lore::Address;
    use crate::lore::RepositoryId;
    use crate::node::Node;
    use crate::repository::RepositoryContext;
    use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
    use crate::repository::test_helpers::default_repository_creation_args;
    use crate::state::ChangeStream;
    use crate::state::FilesystemDiffStats;
    use crate::state::NodeComparison;
    use crate::state::State;
    use crate::util::path::RelativePath;

    #[derive(Default)]
    pub struct TestFilesystemProvider {
        pub begin_count: Arc<AtomicUsize>,
        pub file_info_count: Arc<AtomicUsize>,
        pub holds_name_count: Arc<AtomicUsize>,
        pub names_folding_count: Arc<AtomicUsize>,
        pub finalize_events: Arc<Mutex<Vec<bool>>>,
        finalize_fails: bool,
        write_fails: bool,
        holds_paths: bool,
    }

    impl TestFilesystemProvider {
        pub fn new() -> TestFilesystemProvider {
            Self::default()
        }

        /// A provider whose operations record the finalize and then report it failed.
        pub fn failing_finalize() -> TestFilesystemProvider {
            Self {
                finalize_fails: true,
                ..Self::new()
            }
        }

        /// A provider whose operations report every write they are asked for as failed.
        pub fn failing_writes() -> TestFilesystemProvider {
            Self {
                write_fails: true,
                ..Self::new()
            }
        }

        /// A provider that reports every path as a file it holds, spelled as asked, which is
        /// what a caller resolving a path that exists reads.
        pub fn holding_every_path() -> TestFilesystemProvider {
            Self {
                holds_paths: true,
                ..Self::new()
            }
        }

        pub fn begins(&self) -> usize {
            self.begin_count.load(Ordering::Acquire)
        }

        /// How many paths were looked up through operations this provider began.
        pub fn file_infos(&self) -> usize {
            self.file_info_count.load(Ordering::Acquire)
        }

        /// How many single-name lookups and directory reads a case resolution cost.
        pub fn name_lookups(&self) -> usize {
            self.holds_name_count.load(Ordering::Acquire)
        }

        pub fn directory_reads(&self) -> usize {
            self.names_folding_count.load(Ordering::Acquire)
        }
    }

    /// A repository over `filesystem`, with the stores every context needs.
    pub async fn test_repository(
        filesystem: Arc<TestFilesystemProvider>,
    ) -> Arc<RepositoryContext> {
        let (immutable_store, mutable_store, _context) =
            test_store_create().await.expect("Making test stores");
        Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_filesystem_provider(filesystem),
        ))
    }

    #[async_trait]
    impl FilesystemProvider for TestFilesystemProvider {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            self.begin_count.fetch_add(1, Ordering::AcqRel);
            Ok(Arc::new(InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Test(TestOperation {
                    file_info_count: self.file_info_count.clone(),
                    holds_name_count: self.holds_name_count.clone(),
                    names_folding_count: self.names_folding_count.clone(),
                    finalize_events: self.finalize_events.clone(),
                    finalize_fails: self.finalize_fails,
                    write_fails: self.write_fails,
                    holds_paths: self.holds_paths,
                }),
            )))
        }
    }

    /// Answers the reads and the directory create these tests make, and records each finalize.
    /// Every other member panics.
    pub struct TestOperation {
        file_info_count: Arc<AtomicUsize>,
        holds_name_count: Arc<AtomicUsize>,
        names_folding_count: Arc<AtomicUsize>,
        finalize_events: Arc<Mutex<Vec<bool>>>,
        finalize_fails: bool,
        write_fails: bool,
        holds_paths: bool,
    }

    impl TestOperation {
        /// Records the finalize and the writes it was told the operation made.
        pub(super) fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
            self.finalize_events.lock().push(changes_made);
            if self.finalize_fails {
                return Err(FsError::internal("Finalize failed"));
            }
            Ok(())
        }

        /// What the provider was told to hold, which for the default is a path the filesystem
        /// does not hold.
        fn held(&self) -> FileInfo {
            if self.holds_paths {
                FileInfo::File {
                    executable: None,
                    size: 0,
                    mtime: 0,
                }
            } else {
                FileInfo::NotExist
            }
        }
    }

    impl InstanceOperation for TestOperation {
        /// Reports a working tree holding exactly what the state does, which is what a walk over a
        /// tree with nothing to reconcile answers.
        fn changes_from_filesystem_to_state(
            &self,
            _diff: FilesystemDiffContext,
        ) -> ChangeStream<FilesystemDiffStats> {
            ChangeStream::nothing()
        }

        /// Counts the lookup and reports what the provider was told to hold — what a caller
        /// acts on without needing content behind it.
        async fn file_info(&self, _path: &RelativePath) -> Result<FileInfo, FsError> {
            self.file_info_count.fetch_add(1, Ordering::AcqRel);
            Ok(self.held())
        }

        /// Reports what the provider was told to hold, without counting: the lookup counts
        /// read what went through the tracked tree, which this did not.
        async fn untracked_file_info(&self, _path: &RelativePath) -> Result<FileInfo, FsError> {
            Ok(self.held())
        }

        /// Counts the lookup and reports the spelling asked about as the one held, so a
        /// resolver reading through this one settles a path without reading a directory.
        async fn holds_name_exactly(&self, _path: &RelativePath) -> Option<bool> {
            self.holds_name_count.fetch_add(1, Ordering::AcqRel);
            Some(self.holds_paths)
        }

        async fn names_folding_to(
            &self,
            _path: &RelativePath,
            name: &str,
        ) -> Result<Vec<String>, FsError> {
            self.names_folding_count.fetch_add(1, Ordering::AcqRel);
            Ok(if self.holds_paths {
                vec![name.to_string()]
            } else {
                vec![]
            })
        }

        async fn read_directory(&self, _path: &RelativePath) -> Result<DirectoryListing, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        fn content_source(&self, _path: &RelativePath) -> lore_storage::ContentSource<'static> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn file_holds_content(
            &self,
            _repository: Arc<RepositoryContext>,
            _path: &RelativePath,
            _previous: Address,
            _previous_size: u64,
            _established: &lore_storage::ContentHashes,
        ) -> Result<NodeComparison, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn make_executable(
            &self,
            _path: &RelativePath,
            _executable: bool,
        ) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        /// Succeeds without a tree behind it, or fails where the provider was told writes fail.
        async fn create_dir_all(&self, _path: &RelativePath) -> Result<(), FsError> {
            if self.write_fails {
                return Err(FsError::internal("Write failed"));
            }
            Ok(())
        }

        async fn write_file(&self, _path: &RelativePath, _contents: Bytes) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn rename(&self, _from: &RelativePath, _to: &RelativePath) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn remove(&self, _path: &RelativePath) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn remove_recursive(&self, _path: &RelativePath) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn write_node(
            &self,
            _repository: Arc<RepositoryContext>,
            _node: &Node,
            _path: &RelativePath,
        ) -> Result<FileInfo, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn set_file_to_immutable_store_contents(
            &self,
            _repository: Arc<RepositoryContext>,
            _node: &Node,
            _path: &RelativePath,
        ) -> Result<(Fragment, Option<FileInfo>), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn copy_file(
            &self,
            _source_path: &RelativePath,
            _destination_path: &RelativePath,
        ) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn infer_is_diffable(&self, _path: &RelativePath) -> Result<bool, FsError> {
            panic!("Test operation unimplemented except finalize")
        }
    }

    #[tokio::test]
    async fn one_operation_covers_every_repository_in_the_filesystem() {
        let (immutable_store, mutable_store, _context) =
            test_store_create().await.expect("Making test stores");
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let parent = Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_filesystem_provider(filesystem.clone()),
        ));
        let link = parent.to_link_context(RepositoryId::from([1; 16])).await;

        assert!(
            Arc::ptr_eq(&parent.file_system(), &link.file_system()),
            "A link takes its parent's provider, which is what makes one operation cover both"
        );

        let operation = parent.file_system().begin_operation().await.unwrap();

        assert_eq!(
            1,
            filesystem.begins(),
            "The tree began more than one operation"
        );
        assert_eq!(Vec::<bool>::new(), *(filesystem.finalize_events.lock()));

        operation.finalize().await.expect("Finalize failed");

        assert_eq!(1, filesystem.begins());
        assert_eq!(vec![false], *(filesystem.finalize_events.lock()));
    }

    #[tokio::test]
    async fn work_that_needs_no_operation_opens_none() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let handed: Option<()> =
            with_operation_if(repository.file_system(), false, async |operation| {
                Ok::<_, FsError>(operation.map(|_| ()))
            })
            .await
            .expect("The work succeeded");

        assert!(
            handed.is_none(),
            "Work that needs no operation was handed one"
        );
        assert_eq!(0, filesystem.begins());
        assert!(
            filesystem.finalize_events.lock().is_empty(),
            "An operation that was never opened was finalized"
        );
    }

    #[tokio::test]
    async fn work_that_needs_an_operation_opens_one_and_finalizes_it() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let handed: Option<()> =
            with_operation_if(repository.file_system(), true, async |operation| {
                Ok::<_, FsError>(operation.map(|_| ()))
            })
            .await
            .expect("The work succeeded");

        assert!(
            handed.is_some(),
            "Work that needs an operation was handed none"
        );
        assert_eq!(1, filesystem.begins());
        assert_eq!(vec![false], *(filesystem.finalize_events.lock()));
    }

    #[tokio::test]
    async fn a_failing_operation_is_still_finalized_where_one_was_needed() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation_if(repository.file_system(), true, async |_operation| {
                Err(FsError::internal("Work failed"))
            })
            .await;

        result.expect_err("The work's error should be reported");
        assert_eq!(
            vec![false],
            *(filesystem.finalize_events.lock()),
            "A failed operation was left unfinalized"
        );
    }

    #[tokio::test]
    async fn a_failing_operation_is_still_finalized() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), async |_operation| {
                Err(FsError::internal("Work failed"))
            })
            .await;

        result.expect_err("The work's error should be reported");
        assert_eq!(
            vec![false],
            *(filesystem.finalize_events.lock()),
            "A failed operation was left unfinalized"
        );
    }

    #[tokio::test]
    async fn a_successful_operation_reports_its_value() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let value: u32 = with_operation(repository.file_system(), async |_operation| {
            Ok::<_, FsError>(7)
        })
        .await
        .expect("The work succeeded");

        assert_eq!(7, value);
        assert_eq!(1, filesystem.begins());
        assert_eq!(vec![false], *(filesystem.finalize_events.lock()));
    }

    #[tokio::test]
    async fn an_operation_that_wrote_finalizes_as_changed() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        with_operation(repository.file_system(), async |operation| {
            operation.create_dir_all(&relative("written")).await
        })
        .await
        .expect("The work succeeded");

        assert_eq!(
            vec![true],
            *(filesystem.finalize_events.lock()),
            "A write the operation performed was not reported to the finalize"
        );
    }

    #[tokio::test]
    async fn an_operation_that_only_read_finalizes_as_unchanged() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        with_operation(repository.file_system(), async |operation| {
            operation.file_info(&relative("read")).await.map(|_| ())
        })
        .await
        .expect("The work succeeded");

        assert_eq!(
            vec![false],
            *(filesystem.finalize_events.lock()),
            "A read was reported to the finalize as a write"
        );
    }

    /// A write that failed leaves the same stale cache behind as one that succeeded, so what
    /// the operation was asked for is what it reports.
    #[tokio::test]
    async fn an_operation_whose_write_failed_finalizes_as_changed() {
        let filesystem = Arc::new(TestFilesystemProvider::failing_writes());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), async |operation| {
                operation.create_dir_all(&relative("written")).await
            })
            .await;

        result.expect_err("The write failed");
        assert_eq!(vec![true], *(filesystem.finalize_events.lock()));
    }

    #[tokio::test]
    async fn a_finalize_failure_is_reported_where_the_work_succeeded() {
        let filesystem = Arc::new(TestFilesystemProvider::failing_finalize());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), async |_operation| Ok(())).await;

        result.expect_err("A finalize failure should be reported");
    }

    #[tokio::test]
    async fn the_works_error_is_reported_ahead_of_a_finalize_failure() {
        let filesystem = Arc::new(TestFilesystemProvider::failing_finalize());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), async |_operation| {
                Err(FsError::internal("Work failed"))
            })
            .await;

        let error = result.expect_err("The work failed");
        assert!(
            format!("{error}").contains("Work failed"),
            "The finalize failure displaced the work's error: {error}"
        );
    }

    /// `TestOperation` panics when the walk is reached, so the diff returning at all is
    /// the assertion: a filter-excluded path is answered without an operation.
    #[tokio::test]
    async fn an_excluded_path_reaches_no_operation() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Making test stores");
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                let mut filter = crate::filter::Filter::default();
                filter
                    .ignore
                    .add_exclusion("secret")
                    .expect("exclusion rule");
                let repository = Arc::new(RepositoryContext::new(
                    default_repository_creation_args(immutable_store, mutable_store)
                        .with_filesystem_provider(Arc::new(TestFilesystemProvider::new()))
                        .with_filter(Arc::new(filter)),
                ));
                let operation = repository.file_system().begin_operation().await.unwrap();
                let state = Arc::new(State::new());
                let tree = || FilesystemDiffTree {
                    repository: repository.clone(),
                    state: state.clone(),
                };
                let mut changes = crate::state::diff_filesystem(
                    &operation,
                    tree(),
                    tree(),
                    Some(
                        crate::util::path::RelativePath::new_from_initial_path("secret")
                            .expect("path"),
                    ),
                    crate::filter::FilterMode::Full,
                    FilesystemDiffIntent::Report,
                    Arc::new(Vec::new()),
                )
                .await
                .expect("An excluded path is not an error");

                assert!(
                    changes.next().await.is_none(),
                    "An excluded path reported changes"
                );
                let stats = changes
                    .finish()
                    .await
                    .expect("An excluded path is not an error");
                assert_eq!(0, stats.file_add.load(std::sync::atomic::Ordering::Relaxed));
            })
            .await;
    }

    /// The walk hands this to every component above a staged path, so it has to report
    /// a directory that holds nothing a directory node would store.
    #[test]
    fn a_directory_info_is_an_existing_directory_with_no_content() {
        let info = FileInfo::Directory;
        assert!(info.exists());
        assert!(info.is_dir());
        assert!(!info.is_file());
        assert_eq!(0, info.size());
        assert_eq!(0, info.mtime());
    }

    /// The mode read straight off the metadata, which is the path [`FileInfo::mode`] has to
    /// reproduce for a node staged either way to land the same one.
    fn metadata_to_mode(metadata: &std::fs::Metadata, previous: u16) -> u16 {
        crate::util::fs::mode_from_observed(
            metadata.is_file(),
            crate::util::fs::file_executable_observed(metadata),
            previous,
        )
    }

    /// A node staged from a `FileInfo` has to land the size, time and mode a node
    /// staged from the metadata itself would, since the two are the same walk before
    /// and after the file information became the currency between them. A directory
    /// states none of what a file stores, which is none of what a directory node takes.
    #[test]
    fn file_information_answers_what_the_metadata_helpers_answer() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-test-");
        let path = dir.path().join("file");
        std::fs::write(&path, b"content").expect("write");

        let check = |path: &Path| {
            let metadata = std::fs::metadata(path).expect("metadata");
            let info = FileInfo::from_metadata(&metadata);
            assert_eq!(crate::util::fs::file_size(&metadata), info.size());
            assert_eq!(crate::util::fs::file_mtime(&metadata), info.mtime());
            assert_eq!(metadata.is_dir(), info.is_dir());
            assert_eq!(metadata.is_file(), info.is_file());
            for previous in [0, crate::node::NodeFileMode::Executable.bits()] {
                assert_eq!(
                    metadata_to_mode(&metadata, previous),
                    info.mode(previous),
                    "mode for {} from previous {previous}",
                    path.display()
                );
            }
        };

        check(&path);

        let metadata = std::fs::metadata(dir.path()).expect("metadata");
        let directory = FileInfo::from_metadata(&metadata);
        assert_eq!(FileInfo::Directory, directory);
        for previous in [0, crate::node::NodeFileMode::Executable.bits()] {
            assert_eq!(
                metadata_to_mode(&metadata, previous),
                directory.mode(previous),
                "mode for a directory from previous {previous}"
            );
        }

        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("set executable");
            check(&path);
            let info = FileInfo::from_metadata(&std::fs::metadata(&path).expect("metadata"));
            assert_eq!(
                crate::node::NodeFileMode::Executable.bits(),
                info.mode(0),
                "an executable file reports the bit whatever the node held"
            );
        }
    }

    /// The forwarder routes a lookup to the operation rather than refusing one, which
    /// is what lets a test observe the paths an operation was asked about.
    #[tokio::test]
    async fn a_lookup_through_an_operation_is_counted() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let operation = filesystem.begin_operation().await.expect("an operation");
        let path = RelativePath::new_from_initial_path("a/b").expect("path");

        let info = operation
            .file_info(&path)
            .await
            .expect("a lookup is answered");

        assert!(!info.exists());
        assert_eq!(1, filesystem.file_infos());
    }

    /// An operation rooted at `root`, which is what a caller names its paths against.
    async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
        FilesystemProvider::begin_operation(&crate::fs::os::OsFilesystem::new(root))
            .await
            .expect("beginning an operation over the OS filesystem")
    }

    fn relative(path: &str) -> RelativePath {
        RelativePath::new_from_initial_path(path).expect("relative path")
    }

    #[tokio::test]
    async fn a_missing_directory_is_created_with_its_ancestors() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-empty-dir-");
        let operation = os_operation(dir.path()).await;

        create_empty_directory::<FsError>(&operation, &relative("outer/inner"))
            .await
            .expect("an empty directory is created");

        assert!(dir.path().join("outer").join("inner").is_dir());
    }

    #[tokio::test]
    async fn a_directory_already_there_is_the_state_asked_for() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-empty-dir-");
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir_all(dir.path().join("held")).expect("create directory");

        create_empty_directory::<FsError>(&operation, &relative("held"))
            .await
            .expect("a directory already there is accepted");

        assert!(dir.path().join("held").is_dir());
    }

    /// A file standing where a directory belongs is reported rather than passed over: the
    /// caller holds a directory and has nothing to put in the file's place.
    #[tokio::test]
    async fn a_file_standing_where_the_directory_belongs_is_reported() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-empty-dir-");
        let operation = os_operation(dir.path()).await;
        std::fs::write(dir.path().join("held"), b"content").expect("write file");

        create_empty_directory::<FsError>(&operation, &relative("held"))
            .await
            .expect_err("a file standing there is not the state asked for");

        assert!(
            dir.path().join("held").is_file(),
            "the file standing there was replaced"
        );
    }

    /// Every entry the operation lists at `path`, which is what a caller collecting a directory
    /// before acting on it reads, in name order so a test can name the entry it means.
    async fn read_all(operation: &InstanceOperationImpl, path: &str) -> Vec<DirectoryEntry> {
        let mut listing = operation
            .read_directory(&relative(path))
            .await
            .expect("a directory is listed");
        let mut entries = vec![];
        while let Some(entry) = listing.next().await {
            entries.push(entry.expect("an entry is described"));
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        entries
    }

    #[tokio::test]
    async fn a_directory_lists_the_children_it_holds() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir_all(dir.path().join("held").join("inner")).expect("create directory");
        std::fs::write(dir.path().join("held").join("file.txt"), b"content").expect("write file");

        let entries = read_all(&operation, "held").await;

        assert_eq!(
            vec!["file.txt", "inner"],
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<&str>>()
        );
        assert_eq!(
            7,
            entries[0].info.size(),
            "an entry carries what the listing measured at the name"
        );
        assert_eq!(
            crate::hash::hash_string("file.txt"),
            entries[0].name_hash,
            "an entry carries the hash of the name it names"
        );
        assert!(entries[1].info.is_dir());
    }

    /// A listing describes the children the repository tracks and passes over the rest: a link
    /// is left out rather than described as what it points at.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_listing_passes_over_what_the_repository_holds_nothing_for() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
        let operation = os_operation(dir.path()).await;
        let held = dir.path().join("held");
        std::fs::create_dir_all(&held).expect("create directory");
        std::fs::write(held.join("file.txt"), b"content").expect("write file");
        std::os::unix::fs::symlink(held.join("file.txt"), held.join("link.txt"))
            .expect("create symlink");

        let entries = read_all(&operation, "held").await;

        assert_eq!(
            vec!["file.txt"],
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<&str>>()
        );
    }

    /// A name that is not text is reported rather than passed over: it hashes to a node the tree
    /// does not hold, so nothing can be done with it that is not a guess.
    ///
    /// This covers the listing end to end, which takes a filesystem willing to hold such a name.
    /// One enforcing UTF-8 -- ZFS with `utf8only=on`, APFS -- refuses it in the write below,
    /// before any of the code under test runs, and the test steps aside there rather than
    /// reporting that limit as a failure of the listing.
    /// [`crate::util::fs::tests::a_listed_name_that_is_not_text_is_reported`] covers the same
    /// reporting from an assembled entry, so the invariant stays covered on such a filesystem.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_name_that_is_not_text_is_reported() {
        use std::os::unix::ffi::OsStrExt;

        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
        let operation = os_operation(dir.path()).await;
        let held = dir.path().join("held");
        std::fs::create_dir_all(&held).expect("create directory");
        let name = held.join(std::ffi::OsStr::from_bytes(b"\xff"));
        // Any failure here is the filesystem refusing the name: the write is the same one the
        // neighbouring tests do, so a temp directory that could not be written to at all would
        // take those with it rather than showing up only here.
        if std::fs::write(&name, b"content").is_err() {
            return;
        }

        let mut listing = operation
            .read_directory(&relative("held"))
            .await
            .expect("a directory is listed");

        assert!(
            listing.next().await.expect("an entry").is_err(),
            "a name that is not text is reported"
        );
    }

    #[tokio::test]
    async fn an_empty_directory_lists_nothing() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir_all(dir.path().join("held")).expect("create directory");

        assert!(read_all(&operation, "held").await.is_empty());
    }

    /// A path holding no directory is reported where it is read, rather than answering as a
    /// directory holding nothing.
    #[tokio::test]
    async fn a_path_holding_no_directory_is_reported() {
        let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
        let operation = os_operation(dir.path()).await;
        std::fs::write(dir.path().join("held"), b"content").expect("write file");

        assert!(
            operation.read_directory(&relative("held")).await.is_err(),
            "a file holds no children"
        );
        assert!(
            operation
                .read_directory(&relative("missing"))
                .await
                .is_err(),
            "a path holding nothing holds no children"
        );
    }

    #[tokio::test]
    async fn finalizing_twice_is_refused() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let operation = repository.file_system().begin_operation().await.unwrap();
        operation.finalize().await.expect("Finalize failed");
        operation
            .finalize()
            .await
            .expect_err("A second finalize should be refused");

        assert_eq!(
            vec![false],
            *(filesystem.finalize_events.lock()),
            "The refused finalize reached the filesystem"
        );
    }

    pub async fn test_store_create() -> Result<
        (
            std::sync::Arc<dyn lore_storage::ImmutableStore>,
            std::sync::Arc<dyn lore_storage::MutableStore>,
            std::sync::Arc<crate::interface::ExecutionContext>,
        ),
        lore_storage::StoreError,
    > {
        let execution = setup_test_execution();
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                let immutable = lore_storage::local::immutable_store::create(
                    None::<&str>, /* No on disk path, in-memory only */
                    lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                    false, /* Do not deserialize all buckets on start */
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await?;
                let mutable: std::sync::Arc<dyn lore_storage::MutableStore> =
                    lore_storage::local::mutable_store::create(
                        None::<&str>, /* No on disk path, in-memory only */
                        lore_storage::MutableStoreSettings::default(),
                        immutable.clone(),
                    )
                    .await?;
                Ok((immutable, mutable, crate::lore::execution_context()))
            })
            .await
    }

    pub fn setup_test_execution() -> std::sync::Arc<crate::interface::ExecutionContext> {
        std::sync::Arc::new(crate::interface::ExecutionContext::new_client_with_user_id(
            crate::interface::LoreGlobalArgs::default(),
            crate::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ))
    }
}
