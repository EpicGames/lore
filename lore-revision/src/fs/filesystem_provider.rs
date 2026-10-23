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
use lore_base::error::FileNotFound;
use lore_base::error::InvalidArguments;
use lore_base::error::LocalModifications;
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
use crate::lore_debug;
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
    FileNotFound,
    InvalidArguments,
    LocalModifications,
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
    /// Where a marking walk holds the nodes it finds stale. `None` has the walk hold them itself
    /// and discard them once it has drained; a caller running several walks over one tree hands
    /// each the same queue and discards it once all of them have drained.
    pub discards: Option<Arc<crate::state::WalkDiscards>>,
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

    /// Whether the provider holds the working tree's current state as a `State` it serves reads
    /// from, rather than as files on a filesystem. Its
    /// [`InstanceOperation::changes_from_filesystem_to_state`] then answers every change the
    /// working tree holds without reading it. Every operation it begins answers the same
    /// ([`InstanceOperationImpl::holds_state`]), so a provider handing out another provider's
    /// operations answers as that provider does.
    fn holds_state(&self) -> bool {
        false
    }
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

/// Sets the executable bit at `path` to what `node` holds, where `file_info` reports a bit to
/// compare against. A platform that reports none leaves the file alone.
pub async fn match_node_executable<E>(
    operation: &InstanceOperationImpl,
    path: &RelativePath,
    node: &Node,
    file_info: &FileInfo,
) -> Result<(), E>
where
    E: ErrorSet,
{
    let node_executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
    if file_info
        .executable()
        .is_some_and(|observed| observed != node_executable)
    {
        operation
            .make_executable(path, node_executable)
            .await
            .forward_any_with::<E, _>(|| format!("Failed to set the executable bit of {path}"))?;
    }
    Ok(())
}

/// Sets the file at `path` to what `node` holds, its content and then its executable bit, and
/// reports the fragment the content came from and the file as the content write left it.
///
/// The write reports the file where it captured one on its open handle, and the file is looked
/// up where it did not.
pub async fn set_file_to_node<E>(
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    node: &Node,
    path: &RelativePath,
) -> Result<(Fragment, FileInfo), E>
where
    E: ErrorSet,
{
    let (fragment, written) = operation
        .set_file_to_immutable_store_contents(repository, node.address, path)
        .await
        .forward_any_with::<E, _>(|| format!("Failed to write file {path}"))?;
    let file_info = match written {
        Some(file_info) => file_info,
        None => operation
            .file_info(path)
            .await
            .forward_any_with::<E, _>(|| format!("Failed to read the file written at {path}"))?,
    };
    match_node_executable::<E>(operation, path, node, &file_info).await?;
    Ok((fragment, file_info))
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
    /// source from here rather than naming a host path the provider may not read through. A path
    /// holding no file, nothing or a directory, has no content: a provider that resolves the path
    /// to name a source reports `FileNotFound`, and one that names a host path without reading it
    /// leaves the read to fail. A provider that cannot name the content of a file reports why.
    fn content_source(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<lore_storage::ContentSource<'static>, FsError>> + Send;

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

    /// Sets the file at `path` to the content `address` names, reporting the fragment it came
    /// from and what the file looks like where the write reported it.
    ///
    /// The zero hash addresses no stored content and leaves an empty file, the store holding
    /// nothing to read it from. A caller materializing a tree asks here for every node and does
    /// not sort the empty ones out itself.
    ///
    /// Named by address rather than by node, so a caller holding an address alone -- one writing
    /// out content no revision describes -- reaches the same write.
    ///
    /// The file information is `None` where the write did not report it, which a caller needing
    /// it answers with [`file_info`](Self::file_info).
    fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
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
    /// Content that cannot be read is not diffable, there being nothing to diff, and neither is a
    /// path holding no file. A provider that cannot name the content of a file reports why
    /// ([`content_source`](Self::content_source)).
    ///
    /// Derived from [`content_source`](Self::content_source). A provider that answers without
    /// reading overrides this.
    fn infer_is_diffable(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<bool, FsError>> + Send {
        async move {
            let source = match self.content_source(path).await {
                Ok(source) => source,
                Err(FsError::FileNotFound(_)) => return Ok(false),
                Err(err) => return Err(err),
            };
            Ok(crate::infer::infer_is_diffable(&source)
                .await
                .unwrap_or(false))
        }
    }

    /// Whether the content at `path` holds conflict markers.
    ///
    /// Content that cannot be opened or read holds none, a failed read logged at debug, and neither
    /// does a path holding no file. A provider that cannot name the content of a file reports why
    /// ([`content_source`](Self::content_source)).
    ///
    /// Derived from [`content_source`](Self::content_source). A provider that answers without
    /// reading overrides this.
    fn infer_is_conflicted(
        &self,
        path: &RelativePath,
    ) -> impl Future<Output = Result<bool, FsError>> + Send {
        async move {
            let source = match self.content_source(path).await {
                Ok(source) => source,
                Err(FsError::FileNotFound(_)) => return Ok(false),
                Err(err) => return Err(err),
            };
            Ok(crate::infer::infer_is_conflicted(&source)
                .await
                .unwrap_or_else(|err| {
                    lore_debug!("Conflict check cannot read {path}: {err}");
                    false
                }))
        }
    }

    /// Takes that a commit of the instance's repository stored the content read at `path` through
    /// this operation as `address`, `size` bytes long, with `mode`. The commit tells it of each
    /// file it reads from the working tree, before it hands over the revision holding them
    /// ([`committed`](Self::committed)), and of no view-excluded file, whose content it takes
    /// from the staged node. A dry run, and a commit that fails, tells of the content it stored
    /// and hands nothing over. [`InstanceOperationImpl`] passes it on only where the provider
    /// holds its state in a `State` ([`FilesystemProvider::holds_state`]).
    ///
    /// # Implementation notes
    ///
    /// - **`OsFilesystem`**, **`SWFS`**: nothing, the default.
    /// - A provider that holds its state keeps what each file it served was stored as, which
    ///   `committed` serves the file from without resolving its path again.
    fn stored(&self, _path: &RelativePath, _address: Address, _size: u64, _mode: u16) {}

    /// Takes `committed`, the revision a commit of the instance's repository stored, made current
    /// and flushed, as the revision the working tree stands at, from the files read through this
    /// operation. [`InstanceOperationImpl`] passes it on only where the provider holds its state in
    /// a `State` ([`FilesystemProvider::holds_state`]), outside a dry run, and counts it as a change.
    /// A failure is logged as a warning and the commit stands. Such a provider is refused a commit
    /// of a link alone or of a layer, which would make a revision current that it is not handed.
    ///
    /// `committed` is the State the commit stored, not a copy. Holding on to it past the call is
    /// not recommended: the commit may go on to modify it, reusing it as the staged anchor of the
    /// local changes it left uncommitted.
    ///
    /// # Implementation notes
    ///
    /// - **`OsFilesystem`**, **`SWFS`**: nothing, the default.
    /// - A provider that holds its state serves each file the commit read from `committed` rather
    ///   than from the copy it kept.
    fn committed(
        &self,
        _repository: Arc<RepositoryContext>,
        _committed: Arc<State>,
    ) -> impl Future<Output = Result<(), FsError>> + Send {
        async { Ok(()) }
    }
}

/// Implements `InstanceOperation` by wrapping all other types implementing it and forwarding method
/// calls. This type can then be called into to statically dispatch `InstanceOperation` functions
/// while still not knowing which type is in use at compile time.
pub enum StaticDispatchInstanceOperation {
    Os(OsOperation),
    Swfs(SwfsOperation),
    #[cfg(feature = "test-util")]
    Test(test_util::TestOperation),
}

/// What holding a target revision does to the local changes at the paths it changes
/// ([`InstanceOperationImpl::hold_state`]). Without `overwrite_local` or `keep_local`, a local
/// change at such a path refuses the hold, as it refuses a sync.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldOptions {
    /// Give a local file the target's content, or remove it where the target removes it, as a
    /// forced sync does, whatever `keep_local` asks.
    pub overwrite_local: bool,
    /// Keep each local change, a local deletion included, as a sync forwarding local changes
    /// does.
    pub keep_local: bool,
    /// Check and report what holding the target changes, changing nothing.
    pub dry_run: bool,
}

pub struct InstanceOperationImpl {
    dispatch: StaticDispatchInstanceOperation,
    /// What the provider that began the operation answers for [`FilesystemProvider::holds_state`].
    holds_state: bool,
    /// Whether the operation began in a dry run, which writes nothing through it.
    dry_run: bool,
    finalized: AtomicBool,
    changed: AtomicBool,
    modified_times: RecordedModifiedTimes,
    /// The filesystem clock, read once, the first time [`Self::clock_floor`] is asked.
    clock_floor: tokio::sync::OnceCell<u64>,
}

impl InstanceOperationImpl {
    pub fn new(dispatch: StaticDispatchInstanceOperation, holds_state: bool) -> Self {
        Self {
            dispatch,
            holds_state,
            dry_run: crate::runtime::try_execution_context()
                .is_some_and(|context| context.globals().dry_run()),
            finalized: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            modified_times: RecordedModifiedTimes::default(),
            clock_floor: tokio::sync::OnceCell::new(),
        }
    }

    /// The filesystem clock of `repository`'s working copy as the operation first asked for it,
    /// read then ([`crate::state::read_clock_floor`]): a file the operation reads after asking
    /// may record its time only where it is older, so no write after the read shares it.
    pub async fn clock_floor(&self, repository: &RepositoryContext) -> u64 {
        *self
            .clock_floor
            .get_or_init(|| crate::state::read_clock_floor(repository))
            .await
    }

    /// Collects that `path` holds the content of the node written there, for a caller that
    /// knows which revision the operation leaves current.
    pub fn record_modified_time(
        &self,
        repository: &RepositoryContext,
        path: &RelativePath,
        mtime: u64,
    ) {
        if self.dry_run {
            return;
        }
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
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.finalize(changes_made),
            StaticDispatchInstanceOperation::Os(_this) => Ok(()),
            StaticDispatchInstanceOperation::Swfs(this) => this.finalize(changes_made).await,
        }
    }

    /// Whether the provider that began this operation holds its state in a `State`
    /// ([`FilesystemProvider::holds_state`]).
    pub fn holds_state(&self) -> bool {
        self.holds_state
    }

    /// Hands `target`, under `repository`, its context, to a provider that holds its state in a
    /// `State` ([`FilesystemProvider::holds_state`]) to hold as the revision the working tree
    /// stands at, its local changes refused, kept or overwritten as `options` asks
    /// ([`crate::fs::hold`]). Outside a dry run the hold counts as a change. An operation that
    /// holds no `State` refuses, changing nothing.
    #[allow(clippy::unused_async)]
    pub async fn hold_state(
        &self,
        repository: Arc<RepositoryContext>,
        target: Arc<State>,
        options: HoldOptions,
    ) -> Result<(), FsError> {
        #[cfg(feature = "test-util")]
        if let StaticDispatchInstanceOperation::Test(this) = &self.dispatch
            && self.holds_state
        {
            if !options.dry_run {
                self.record_change();
            }
            return this.hold_state(&target, options);
        }
        let _ = (repository, target, options);
        Err(FsError::internal(
            "Unable to hold a revision through an operation that holds no State",
        ))
    }
}

impl InstanceOperation for InstanceOperationImpl {
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        match &self.dispatch {
            #[cfg(feature = "test-util")]
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
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.file_info(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.file_info(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.file_info(path).await,
        }
    }

    async fn untracked_file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.untracked_file_info(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.untracked_file_info(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.untracked_file_info(path).await,
        }
    }

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        match &self.dispatch {
            #[cfg(feature = "test-util")]
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
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.names_folding_to(path, name).await,
            StaticDispatchInstanceOperation::Os(this) => this.names_folding_to(path, name).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.names_folding_to(path, name).await,
        }
    }

    async fn read_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.read_directory(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.read_directory(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.read_directory(path).await,
        }
    }

    async fn content_source(
        &self,
        path: &RelativePath,
    ) -> Result<lore_storage::ContentSource<'static>, FsError> {
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.content_source(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.content_source(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.content_source(path).await,
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
            #[cfg(feature = "test-util")]
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
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
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
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.create_dir_all(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.create_dir_all(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.create_dir_all(path).await,
        }
    }

    async fn write_file(&self, path: &RelativePath, contents: Bytes) -> Result<(), FsError> {
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.write_file(path, contents).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.write_file(path, contents).await,
        }
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.rename(from, to).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.rename(from, to).await,
        }
    }

    async fn remove(&self, path: &RelativePath) -> Result<(), FsError> {
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.remove(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.remove(path).await,
        }
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
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
        if self.dry_run {
            return Ok(FileInfo::from_node_and_mtime(node, 0));
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
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
        address: Address,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        if self.dry_run {
            return Ok((Fragment::default(), None));
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.set_file_to_immutable_store_contents(repository, address, path)
                    .await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.set_file_to_immutable_store_contents(repository, address, path)
                    .await
            }
        }
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        if self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
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
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.infer_is_diffable(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.infer_is_diffable(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.infer_is_diffable(path).await,
        }
    }

    async fn infer_is_conflicted(&self, path: &RelativePath) -> Result<bool, FsError> {
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.infer_is_conflicted(path).await,
            StaticDispatchInstanceOperation::Os(this) => this.infer_is_conflicted(path).await,
            StaticDispatchInstanceOperation::Swfs(this) => this.infer_is_conflicted(path).await,
        }
    }

    fn stored(&self, path: &RelativePath, address: Address, size: u64, mode: u16) {
        if !self.holds_state {
            return;
        }
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => this.stored(path, address, size, mode),
            StaticDispatchInstanceOperation::Os(this) => this.stored(path, address, size, mode),
            StaticDispatchInstanceOperation::Swfs(this) => this.stored(path, address, size, mode),
        }
    }

    async fn committed(
        &self,
        repository: Arc<RepositoryContext>,
        committed: Arc<State>,
    ) -> Result<(), FsError> {
        if !self.holds_state || self.dry_run {
            return Ok(());
        }
        self.record_change();
        match &self.dispatch {
            #[cfg(feature = "test-util")]
            StaticDispatchInstanceOperation::Test(this) => {
                this.committed(repository, committed).await
            }
            StaticDispatchInstanceOperation::Os(this) => {
                this.committed(repository, committed).await
            }
            StaticDispatchInstanceOperation::Swfs(this) => {
                this.committed(repository, committed).await
            }
        }
    }
}

/// The operation behind [`StaticDispatchInstanceOperation::Test`], which the crate's tests
/// begin from a filesystem provider of their own.
#[cfg(feature = "test-util")]
pub mod test_util {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use bytes::Bytes;
    use lore_base::error::FileNotFound;
    use lore_base::error::LocalModifications;
    use lore_base::types::Fragment;
    use lore_error_set::prelude::*;
    use parking_lot::Mutex;

    use crate::fs::filesystem_provider::DirectoryListing;
    use crate::fs::filesystem_provider::FileInfo;
    use crate::fs::filesystem_provider::FilesystemDiffContext;
    use crate::fs::filesystem_provider::FsError;
    use crate::fs::filesystem_provider::HoldOptions;
    use crate::fs::filesystem_provider::InstanceOperation;
    use crate::lore::Address;
    use crate::lore::Hash;
    use crate::node::Node;
    use crate::node::NodeID;
    use crate::repository::RepositoryContext;
    use crate::state::ChangeStream;
    use crate::state::FilesystemDiffStats;
    use crate::state::NodeComparison;
    use crate::state::State;
    use crate::state::StateError;
    use crate::util::path::RelativePath;

    /// Each `hold_state` call recorded as `(target revision, options)`.
    pub type HoldCalls = Arc<Mutex<Vec<(Hash, HoldOptions)>>>;

    /// Each `committed` call recorded as `(revision handed over, current revision at the call,
    /// whether the State handed over holds no staged or dirty node)`.
    pub type CommittedCalls = Arc<Mutex<Vec<(Hash, Hash, bool)>>>;

    /// Each `stored` call recorded as `(path, address, size, mode, whether a revision had been
    /// handed over before the call)`.
    pub type StoredCalls = Arc<Mutex<Vec<(String, Address, u64, u16, bool)>>>;

    /// Answers the reads and the directory create these tests make, and records each finalize,
    /// `stored` and `committed` call and hold. Every other member panics.
    pub struct TestOperation {
        pub file_info_count: Arc<AtomicUsize>,
        pub holds_name_count: Arc<AtomicUsize>,
        pub names_folding_count: Arc<AtomicUsize>,
        pub finalize_events: Arc<Mutex<Vec<bool>>>,
        pub finalize_fails: bool,
        pub write_fails: bool,
        pub holds_paths: bool,
        /// Whether the provider cannot name the content at any path.
        pub content_source_fails: bool,
        /// Whether the provider holds no file at any path.
        pub content_source_missing: bool,
        pub hold_calls: HoldCalls,
        pub stored_calls: StoredCalls,
        pub committed_calls: CommittedCalls,
        /// Whether each `committed` call fails.
        pub committed_fails: bool,
        /// The operation file information and content are read through, where given.
        pub reads: Option<crate::fs::os::OsOperation>,
        /// Whether each hold is refused for local changes.
        pub hold_refuses: bool,
        /// Paths whose scan fails, as one over a working tree that cannot be read does.
        pub failing_scans: Arc<Mutex<Vec<String>>>,
        /// Paths whose scan finds the named node of the tree it walks stale, and queues it for
        /// discarding where it is handed a queue.
        pub stale_on_scan: Arc<Mutex<Vec<(String, NodeID)>>>,
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

        /// Records the target revision and the options it was given, rather than acting on
        /// either, having no State to hold, then refuses for local changes where told to.
        pub(super) fn hold_state(
            &self,
            target: &State,
            options: HoldOptions,
        ) -> Result<(), FsError> {
            self.hold_calls.lock().push((target.revision(), options));
            if self.hold_refuses {
                return Err(LocalModifications.into());
            }
            Ok(())
        }
    }

    impl InstanceOperation for TestOperation {
        /// Reports a working tree holding exactly what the state does, which is what a walk over a
        /// tree with nothing to reconcile answers, except where the provider was told the path's
        /// scan fails or finds a node stale.
        fn changes_from_filesystem_to_state(
            &self,
            diff: FilesystemDiffContext,
        ) -> ChangeStream<FilesystemDiffStats> {
            let path = diff.filesystem_path.as_str();
            if let Some(discards) = diff.discards.as_ref() {
                let stale: Vec<NodeID> = self
                    .stale_on_scan
                    .lock()
                    .iter()
                    .filter(|(stale_path, _)| stale_path == path)
                    .map(|(_, node)| *node)
                    .collect();
                discards.queue(diff.from.state.clone(), diff.from.repository.clone(), stale);
            }
            if self
                .failing_scans
                .lock()
                .iter()
                .any(|failing| failing == path)
            {
                return ChangeStream::spawn(|_changes| async {
                    Err(StateError::internal("The working tree cannot be read"))
                });
            }
            ChangeStream::nothing()
        }

        /// Counts the lookup, and reports what the directory it reads from holds where given and
        /// otherwise what the provider was told to hold.
        async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
            self.file_info_count.fetch_add(1, Ordering::AcqRel);
            match &self.reads {
                Some(reads) => reads.file_info(path).await,
                None => Ok(self.held()),
            }
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

        /// The file under the directory the provider reads from. Reports no file where the
        /// provider was told it holds none, fails where it was told it cannot name content, and
        /// panics where it reads from no directory.
        async fn content_source(
            &self,
            path: &RelativePath,
        ) -> Result<lore_storage::ContentSource<'static>, FsError> {
            if self.content_source_missing {
                return Err(FileNotFound {
                    resource: path.to_string(),
                }
                .into());
            }
            if self.content_source_fails {
                return Err(FsError::internal("Content source failed"));
            }
            match &self.reads {
                Some(reads) => reads.content_source(path).await,
                None => panic!("Test operation unimplemented except finalize"),
            }
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
            _address: Address,
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

        /// Records what was stored, and whether a revision had been handed over before.
        fn stored(&self, path: &RelativePath, address: Address, size: u64, mode: u16) {
            let handed_over = !self.committed_calls.lock().is_empty();
            self.stored_calls.lock().push((
                path.as_str().to_owned(),
                address,
                size,
                mode,
                handed_over,
            ));
        }

        /// Records the revision it was given, the current revision at the call and whether the
        /// State is clean, and fails where the provider was told to.
        async fn committed(
            &self,
            repository: Arc<RepositoryContext>,
            committed: Arc<State>,
        ) -> Result<(), FsError> {
            let (current, _) = crate::instance::load_current_anchor(&repository)
                .await
                .forward_any::<FsError>("loading the current anchor")?;
            let staged_or_dirty = committed
                .node_has_staged_or_dirty_children(repository, crate::node::ROOT_NODE)
                .await
                .forward_any::<FsError>("reading the committed State")?;
            self.committed_calls
                .lock()
                .push((committed.revision(), current, !staged_or_dirty));
            if self.committed_fails {
                return Err(FsError::internal("Taking the committed revision failed"));
            }
            Ok(())
        }
    }
}
