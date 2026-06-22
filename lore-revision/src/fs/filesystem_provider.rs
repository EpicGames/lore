// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Core filesystem provider traits for repository operations.
//!
//! This module defines the two-trait architecture that separates operation context creation
//! (freeze for SWFS) from actual file operations (work against frozen snapshot).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::Address;
use lore_error_set::error_set;

use crate::change::NodeChange;
use crate::filter::FilterMode;
use crate::fs::os::OsOperation;
use crate::lore::Hash;
use crate::merge::MergeTextMode;
use crate::node::Node;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::FilesystemDiffStats;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum FsError {}

impl From<std::io::Error> for FsError {
    fn from(value: std::io::Error) -> Self {
        Self::internal(value.to_string())
    }
}

/// Basic file information returned by `InstanceOperation::file_info`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileInfo {
    /// Whether the path exists on the filesystem.
    pub exists: bool,
    /// Whether the path is a file (false if directory or doesn't exist).
    pub is_file: bool,
    /// Whether the path is a directory.
    pub is_dir: bool,
    /// Whether the file is executable.
    pub executable: bool,
    /// File size in bytes (0 if doesn't exist or is directory).
    pub size: u64,
    /// Modification time as Unix timestamp in milliseconds.
    pub mtime: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileDifferenceFromNode {
    /// Whether the file content differs from the node.
    pub modified: bool,
    /// Hash of the file if computed during the modification check.
    pub hash: Hash,
}

/// Result of checking whether a file differs from a node.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileModifiedCheck {
    /// Basic file information.
    pub info: FileInfo,
    /// If the file Merkle tree State had a Node for this file it is included.
    pub from_node: Option<Node>,
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
    /// This must not be called a second time until
    ///
    /// # Implementation notes
    ///
    /// - **`OsFilesystem`**: Returns a lightweight wrapper with no state.
    /// - **`SWFS`**: Freezes the filesystem, creates a snapshot, returns operations that work
    ///   against the snapshot.
    async fn begin_operation(&self) -> Result<Arc<StaticDispatchInstanceOperation>, FsError>;
}

/// A path that can be either relative to the repository root or an absolute scratch path.
///
/// Use `Repository` for paths within the working directory, and `Scratch` for temporary
/// paths outside the repository (e.g., diff scratch directories).
pub enum FilesystemPath<'a> {
    /// A path relative to the repository root.
    Repository(&'a RelativePath),
    /// An absolute path outside the repository (scratch/temp files).
    Scratch(&'a Path),
}

impl<'a> FilesystemPath<'a> {
    /// Convert this path to an absolute path given the repository root.
    pub fn to_absolute(&self, repo_path: &Path) -> PathBuf {
        match self {
            FilesystemPath::Repository(rel) => rel.to_absolute_path(repo_path),
            FilesystemPath::Scratch(abs) => abs.to_path_buf(),
        }
    }
}

impl<'a> From<&'a RelativePath> for FilesystemPath<'a> {
    fn from(path: &'a RelativePath) -> Self {
        FilesystemPath::Repository(path)
    }
}

impl<'a> From<&'a Path> for FilesystemPath<'a> {
    fn from(path: &'a Path) -> Self {
        FilesystemPath::Scratch(path)
    }
}

/// Instance operation trait - performs file operations within a context.
///
/// Operations are performed against a consistent snapshot (for SWFS) or directly
/// against the filesystem (for OS-backed).
///
/// This type is not dyn-safe, async methods don't have their future boxed to allow static dispatch
/// though an `impl InstanceOperation`
pub trait InstanceOperation: Send + Sync {
    /// Compute differences between the given state and the current filesystem.
    ///
    /// Returns a Vec of `NodeChange` describing what changed:
    /// - Files added on disk but not in state
    /// - Files modified on disk vs. their state content hash
    /// - Files deleted from disk but present in state
    /// - Metadata changes (permissions, etc.)
    ///
    /// TODO(UCS-19486): Stream results rather than return a single Vec
    #[allow(clippy::too_many_arguments)]
    fn changes_from_filesystem_to_state(
        &self,
        repository_from: Arc<RepositoryContext>,
        state_from: Arc<State>,
        repository_current: Arc<RepositoryContext>,
        state_current: Arc<State>,
        node_path: RelativePath,
        root_node_from: NodeID,
        root_node_to: NodeID,
        filter_mode: FilterMode,
    ) -> impl Future<Output = Result<(Vec<NodeChange>, FilesystemDiffStats), FsError>> + Send;

    /// Get basic file information for a path.
    ///
    /// Returns file existence, type, size, mtime, and mode without checking
    /// content modification against a node.
    fn file_info(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send;

    /// Check if a file on the filesystem differs from a node in state.
    ///
    /// This method combines metadata retrieval and content comparison into a single
    /// operation. It returns information about the filesystem path's existence, type,
    /// and size, as well as whether its content differs from the given node.
    ///
    /// # Arguments
    ///
    /// * `repository` - Repository context for timestamp tracking and content hashing
    /// * `node` - The node to compare against (may be a file or directory node)
    /// * `path` - Relative path within the repository
    /// * `force_full_check` - If true, always compare against the ground truth; if false, use early
    ///   return optimizations that rely on signals like file modification time
    ///
    /// # Returns
    ///
    /// A `FileModifiedCheck`
    fn is_file_modified(
        &self,
        repository: Arc<RepositoryContext>,
        node_change: &NodeChange,
        force_full_check: bool,
    ) -> impl Future<Output = Result<FileModifiedCheck, FsError>> + Send;

    /// Gets the hash of a file in the repository, optionally providing the Node if it has
    /// separately been loaded.
    fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        node_hint: Option<&Node>,
    ) -> impl Future<Output = Result<Hash, FsError>> + Send;

    /// Compare if an address matches what's on disk.
    ///
    /// # Arguments
    /// * `known_disk_file_size` - The size of the file on disk, which should have been accessed by
    ///   the caller.
    fn file_compare(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
        known_disk_file_size: u64,
    ) -> impl Future<Output = Result<bool, FsError>> + Send;

    /// Make a file executable (Unix) or set executable bit equivalent.
    ///
    /// On Windows, this is a no-op.
    fn make_executable(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Create a directory if it doesn't exist (mkdir -p behavior).
    fn create_dir_all(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Create an empty file.
    fn create_file(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Changes the casing of a file from `from` to `to` based on various OS and command argument
    /// settings. `to` must be identical to `from` other than case differences.
    fn unify_case_rename(
        &self,
        from: FilesystemPath<'_>,
        to: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Delete a file or empty directory.
    fn remove(&self, path: FilesystemPath<'_>) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Delete a directory and all contents.
    fn remove_recursive(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Sets the file at `path` to be the contents of `Node`.
    fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Copy the contents of `source_path` to `destination_path`, with the destination being a
    /// scratch file that is not expected to be part of the repository even if it's in its path.
    fn copy_to_scratch_file(
        &self,
        source_path: FilesystemPath<'_>,
        destination_path: impl AsRef<Path> + Send,
    ) -> impl Future<Output = Result<(), FsError>> + Send;

    /// Merge 3 files that exist on the file system.
    fn merge3_text_by_path(
        &self,
        base: &RelativePath,
        mine: &RelativePath,
        theirs: &RelativePath,
        result: &RelativePath,
        mode: MergeTextMode<'_>,
    ) -> impl Future<Output = Result<bool, FsError>> + Send;

    /// Load the contents of `path` to see if it can be diffed or must only be opaquely compared.
    fn infer_is_diffable(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<bool, FsError>> + Send;

    /// Finalize the operation.
    ///
    /// # Parameters
    ///
    /// - `changes_made`: Reports whether changes were made to the file system during the operation.
    ///
    /// On SWFS this clears the cache to enable those writes.
    ///
    /// # Implementation notes
    ///
    /// - **`OsOperation`**: No-op (returns immediately).
    /// - **`SWFS`**: Thaws the filesystem, optionally clears the write cache based on `changes_made`.
    fn finalize(&self, changes_made: bool) -> impl Future<Output = Result<(), FsError>> + Send;
}

/// Implements `InstanceOperation` by wrapping all other types implementing it and forwarding method
/// calls. This type can then be called into to statically dispatch `InstanceOperation` functions
/// while still not knowing which type is in use at compile time.
pub enum StaticDispatchInstanceOperation {
    Os(OsOperation),
}

impl InstanceOperation for StaticDispatchInstanceOperation {
    async fn changes_from_filesystem_to_state(
        &self,
        repository_from: Arc<RepositoryContext>,
        state_from: Arc<State>,
        repository_current: Arc<RepositoryContext>,
        state_current: Arc<State>,
        node_path: RelativePath,
        root_node_from: NodeID,
        root_node_to: NodeID,
        filter_mode: FilterMode,
    ) -> Result<(Vec<NodeChange>, FilesystemDiffStats), FsError> {
        match self {
            Self::Os(this) => {
                this.changes_from_filesystem_to_state(
                    repository_from,
                    state_from,
                    repository_current,
                    state_current,
                    node_path,
                    root_node_from,
                    root_node_to,
                    filter_mode,
                )
                .await
            }
        }
    }

    async fn file_info(&self, path: FilesystemPath<'_>) -> Result<FileInfo, FsError> {
        match self {
            Self::Os(this) => this.file_info(path).await,
        }
    }

    async fn is_file_modified(
        &self,
        repository: Arc<RepositoryContext>,
        node_change: &NodeChange,
        force_full_check: bool,
    ) -> Result<FileModifiedCheck, FsError> {
        match self {
            Self::Os(this) => {
                this.is_file_modified(repository, node_change, force_full_check)
                    .await
            }
        }
    }

    async fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        node_hint: Option<&Node>,
    ) -> Result<Hash, FsError> {
        match self {
            Self::Os(this) => this.file_hash(repository, path, node_hint).await,
        }
    }

    async fn file_compare(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
        known_disk_file_size: u64,
    ) -> Result<bool, FsError> {
        match self {
            Self::Os(this) => {
                this.file_compare(repository, address, path, known_disk_file_size)
                    .await
            }
        }
    }

    async fn make_executable(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.make_executable(path).await,
        }
    }

    async fn create_dir_all(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.create_dir_all(path).await,
        }
    }

    async fn create_file(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.create_file(path).await,
        }
    }

    async fn unify_case_rename(
        &self,
        from: FilesystemPath<'_>,
        to: FilesystemPath<'_>,
    ) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.unify_case_rename(from, to).await,
        }
    }

    async fn remove(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.remove(path).await,
        }
    }

    async fn remove_recursive(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.remove_recursive(path).await,
        }
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: FilesystemPath<'_>,
    ) -> Result<(), FsError> {
        match self {
            Self::Os(this) => {
                this.set_file_to_immutable_store_contents(repository, node, path)
                    .await
            }
        }
    }

    async fn copy_to_scratch_file(
        &self,
        source_path: FilesystemPath<'_>,
        destination_path: impl AsRef<Path> + Send,
    ) -> Result<(), FsError> {
        match self {
            Self::Os(this) => {
                this.copy_to_scratch_file(source_path, destination_path)
                    .await
            }
        }
    }

    async fn merge3_text_by_path(
        &self,
        base: &RelativePath,
        mine: &RelativePath,
        theirs: &RelativePath,
        result: &RelativePath,
        mode: MergeTextMode<'_>,
    ) -> Result<bool, FsError> {
        match self {
            Self::Os(this) => {
                this.merge3_text_by_path(base, mine, theirs, result, mode)
                    .await
            }
        }
    }

    async fn infer_is_diffable(&self, path: FilesystemPath<'_>) -> Result<bool, FsError> {
        match self {
            Self::Os(this) => this.infer_is_diffable(path).await,
        }
    }

    async fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
        match self {
            Self::Os(this) => this.finalize(changes_made).await,
        }
    }
}
