// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Core filesystem provider traits for repository operations.
//!
//! This module defines the two-trait architecture that separates operation context creation
//! (freeze for SWFS) from actual file operations (work against frozen snapshot).

use std::fs::Metadata;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore_base::error::InvalidArguments;
use lore_base::types::Fragment;
use lore_error_set::ErrorSet;
use lore_error_set::error_set;
use lore_error_set::prelude::*;

use crate::change::NodeChange;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::os::OsOperation;
use crate::lore::Hash;
use crate::merge::MergeTextMode;
use crate::node::Node;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::FilesystemDiffStats;
use crate::state::LayerMountInfo;
use crate::state::LinkMountInfo;
use crate::state::NodeComparison;
use crate::state::RecordedModifiedTimes;
use crate::state::State;
use crate::util::path::RelativePath;
use crate::util::path::RepositoryPath;

#[error_set]
pub enum FsError {
    InvalidArguments,
}

impl From<std::io::Error> for FsError {
    fn from(value: std::io::Error) -> Self {
        FsError::internal(value.to_string())
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
    /// Whether the file carries the executable bit, `None` where the platform has no
    /// such bit to read. See [`FileInfo::mode`].
    pub executable: Option<bool>,
    /// File size in bytes (0 if doesn't exist or is directory).
    pub size: u64,
    /// Modification time as Unix timestamp in milliseconds.
    pub mtime: u64,
}

impl FileInfo {
    pub fn from_metadata(metadata: Metadata) -> Self {
        let (mtime, size) = crate::util::fs::file_mtime_and_size(&metadata);
        let executable = crate::util::fs::file_executable_observed(&metadata);
        FileInfo {
            exists: true,
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            executable,
            size,
            mtime,
        }
    }

    /// The mode to store on a node whose mode is `previous`, as
    /// [`crate::util::fs::metadata_to_mode`] answers it for the metadata this was read
    /// from.
    pub fn mode(&self, previous: u16) -> u16 {
        crate::util::fs::mode_from_observed(self.is_file, self.executable, previous)
    }
}

/// One side of a filesystem diff: which tree, rooted where. `node_path` is where
/// `root_node` sits in its own tree, which differs from the path being walked once a
/// link or layer mount has been crossed.
pub struct FilesystemTraversal {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub node_path: RelativePath,
    pub root_node: NodeID,
}

/// A tree to diff against, before a path in it is resolved to a root. Resolving one
/// yields the [`FilesystemTraversal`] the diff walks.
pub struct FilesystemDiffTree {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
}

/// What a filesystem diff does with the differences it finds.
#[derive(Debug, Clone, Copy)]
pub enum FilesystemDiffIntent {
    /// Report them, leaving the trees untouched.
    Report,
    /// Set and clear `Dirty` on each node as the walk settles it.
    MarkDirty,
}

impl FilesystemDiffIntent {
    /// Whether the walk persists what it finds as dirty flags rather than only reporting.
    pub fn marks_dirty(self) -> bool {
        matches!(self, FilesystemDiffIntent::MarkDirty)
    }
}

/// What to diff against the filesystem: `from` is the tree it is compared against and
/// `current` is what the working copy last held, which is how an unstaged add is told
/// apart from a tracked file.
pub struct FilesystemDiffContext {
    pub from: FilesystemTraversal,
    pub current: FilesystemTraversal,
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
#[derive(Debug, Clone, Copy, Default)]
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
/// `changes_made` reports whether the work wrote to the filesystem. The work's error is
/// reported ahead of a finalize failure, being the one that explains the run.
pub async fn with_operation<T, E, F>(
    filesystem: Arc<dyn FilesystemProvider>,
    changes_made: bool,
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
        .finalize(changes_made)
        .await
        .forward_any::<E>("Failed to finish filesystem operation");
    let value = result?;
    finalized?;
    Ok(value)
}

/// A path that can be either relative to the repository root or an absolute scratch path.
///
/// Use `Repository` for paths within the working directory, and `Scratch` for temporary
/// paths outside the repository (e.g., diff scratch directories).
#[derive(Clone, Copy)]
pub enum FilesystemPath<'a> {
    /// A path relative to the repository root.
    Repository(&'a RepositoryPath),
    /// An absolute path outside the repository (scratch/temp files).
    Scratch(&'a Path),
}

impl<'a> FilesystemPath<'a> {
    pub fn from_repository(path: &'a RepositoryPath) -> Self {
        FilesystemPath::Repository(path)
    }

    pub fn from_scratch_path(absolute_path: &'a Path) -> Self {
        Self::Scratch(absolute_path)
    }

    pub fn as_absolute_path(&self) -> &Path {
        match self {
            FilesystemPath::Repository(path) => path.absolute(),
            FilesystemPath::Scratch(abs) => abs,
        }
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
    /// Diff the filesystem under `diff.filesystem_path` against the trees it names,
    /// pushing a change per difference onto `changes`.
    ///
    /// Reports files added, modified or deleted on disk, and metadata changes.
    /// `diff.intent` decides whether the trees are marked as it goes.
    ///
    /// TODO(UCS-19486): Stream results rather than fill a Vec
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
        changes: &mut Vec<NodeChange>,
    ) -> impl Future<Output = Result<FilesystemDiffStats, FsError>> + Send;

    /// Get basic file information for a path.
    ///
    /// Returns file existence, type, size, mtime, and mode without checking
    /// content modification against a node.
    fn file_info(
        &self,
        path: FilesystemPath<'_>,
    ) -> impl Future<Output = Result<FileInfo, FsError>> + Send;

    /// Gets the hash of a file in the repository, optionally providing the Node if it has
    /// separately been loaded.
    fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: FilesystemPath<'_>,
        node_hint: Option<&Node>,
    ) -> impl Future<Output = Result<Hash, FsError>> + Send;

    /// How the file at `path` compares to the content `node` addresses.
    ///
    /// Takes the node to compare against rather than deriving it from a change, so a caller
    /// holding both sides of a change can ask about either. Compares content rather than
    /// consulting a recorded modification time, which speaks only for the current revision's
    /// node and so cannot answer for the other side of a change. A file that cannot be read
    /// is reported as such rather than as either answer, so a caller does not act on a
    /// comparison that never happened.
    fn compare_file_to_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
        file_size: u64,
        content: &lore_storage::ContentHashMemo<'_>,
    ) -> impl Future<Output = Result<NodeComparison, FsError>> + Send;

    /// Make a file executable (Unix) or set executable bit equivalent.
    ///
    /// On Windows, this is a no-op.
    fn make_executable(
        &self,
        path: FilesystemPath<'_>,
        executable: bool,
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
    ) -> impl Future<Output = Result<(Fragment, Option<FileInfo>), FsError>> + Send;

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
    #[cfg(test)]
    Test(tests::TestOperation),
}

pub struct InstanceOperationImpl {
    dispatch: StaticDispatchInstanceOperation,
    finalized: AtomicBool,
    modified_times: RecordedModifiedTimes,
}

impl InstanceOperationImpl {
    pub fn new(dispatch: StaticDispatchInstanceOperation) -> Self {
        Self {
            dispatch,
            finalized: AtomicBool::new(false),
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
}

impl InstanceOperation for InstanceOperationImpl {
    async fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
        changes: &mut Vec<NodeChange>,
    ) -> Result<FilesystemDiffStats, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.changes_from_filesystem_to_state(diff, changes).await
            }
        }
    }

    async fn file_info(&self, path: FilesystemPath<'_>) -> Result<FileInfo, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.file_info(path).await,
        }
    }

    async fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: FilesystemPath<'_>,
        node_hint: Option<&Node>,
    ) -> Result<Hash, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.file_hash(repository, path, node_hint).await
            }
        }
    }

    async fn compare_file_to_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
        file_size: u64,
        content: &lore_storage::ContentHashMemo<'_>,
    ) -> Result<NodeComparison, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.compare_file_to_node(repository, node, path, file_size, content)
                    .await
            }
        }
    }

    async fn make_executable(
        &self,
        path: FilesystemPath<'_>,
        executable: bool,
    ) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.make_executable(path, executable).await
            }
        }
    }

    async fn create_dir_all(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.create_dir_all(path).await,
        }
    }

    async fn create_file(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.create_file(path).await,
        }
    }

    async fn unify_case_rename(
        &self,
        from: FilesystemPath<'_>,
        to: FilesystemPath<'_>,
    ) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.unify_case_rename(from, to).await,
        }
    }

    async fn remove(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.remove(path).await,
        }
    }

    async fn remove_recursive(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.remove_recursive(path).await,
        }
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: FilesystemPath<'_>,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
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
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
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
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.merge3_text_by_path(base, mine, theirs, result, mode)
                    .await
            }
        }
    }

    async fn infer_is_diffable(&self, path: FilesystemPath<'_>) -> Result<bool, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.infer_is_diffable(path).await,
        }
    }

    async fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
        if !self.claim_finalize() {
            return Err(FsError::internal("Operation already finalized"));
        }
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.finalize(changes_made).await,
            StaticDispatchInstanceOperation::Os(this) => this.finalize(changes_made).await,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use lore_base::types::Fragment;
    use parking_lot::Mutex;

    use crate::change::NodeChange;
    use crate::fs::filesystem_provider::FileInfo;
    use crate::fs::filesystem_provider::FilesystemDiffContext;
    use crate::fs::filesystem_provider::FilesystemDiffIntent;
    use crate::fs::filesystem_provider::FilesystemDiffTree;
    use crate::fs::filesystem_provider::FilesystemPath;
    use crate::fs::filesystem_provider::FilesystemProvider;
    use crate::fs::filesystem_provider::FsError;
    use crate::fs::filesystem_provider::InstanceOperation;
    use crate::fs::filesystem_provider::InstanceOperationImpl;
    use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
    use crate::fs::filesystem_provider::with_operation;
    use crate::lore::Hash;
    use crate::lore::RepositoryId;
    use crate::merge::MergeTextMode;
    use crate::node::Node;
    use crate::repository::RepositoryContext;
    use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
    use crate::repository::test_helpers::default_repository_creation_args;
    use crate::state::FilesystemDiffStats;
    use crate::state::NodeComparison;
    use crate::state::State;
    use crate::util::path::RelativePath;

    #[derive(Default)]
    pub struct TestFilesystemProvider {
        pub begin_count: Arc<AtomicUsize>,
        pub finalize_events: Arc<Mutex<Vec<bool>>>,
        finalize_fails: bool,
    }

    impl TestFilesystemProvider {
        pub fn new() -> TestFilesystemProvider {
            Self {
                begin_count: Arc::new(AtomicUsize::new(0)),
                finalize_events: Arc::new(Mutex::new(Vec::new())),
                finalize_fails: false,
            }
        }

        /// A provider whose operations record the finalize and then report it failed.
        pub fn failing_finalize() -> TestFilesystemProvider {
            Self {
                finalize_fails: true,
                ..Self::new()
            }
        }

        pub fn begins(&self) -> usize {
            self.begin_count.load(Ordering::Acquire)
        }
    }

    /// A repository over `filesystem`, with the stores every context needs.
    async fn test_repository(filesystem: Arc<TestFilesystemProvider>) -> Arc<RepositoryContext> {
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
                    finalize_events: self.finalize_events.clone(),
                    finalize_fails: self.finalize_fails,
                }),
            )))
        }
    }

    pub struct TestOperation {
        finalize_events: Arc<Mutex<Vec<bool>>>,
        finalize_fails: bool,
    }

    impl InstanceOperation for TestOperation {
        /// The only actually implemented member, the rest are unimplemented which will fail any
        /// test that calls them.
        async fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
            self.finalize_events.lock().push(changes_made);
            if self.finalize_fails {
                return Err(FsError::internal("Finalize failed"));
            }
            Ok(())
        }

        async fn changes_from_filesystem_to_state(
            &self,
            _diff: FilesystemDiffContext,
            _changes: &mut Vec<NodeChange>,
        ) -> Result<FilesystemDiffStats, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn file_info(&self, _path: FilesystemPath<'_>) -> Result<FileInfo, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn file_hash(
            &self,
            _repository: Arc<RepositoryContext>,
            _path: FilesystemPath<'_>,
            _node_hint: Option<&Node>,
        ) -> Result<Hash, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn compare_file_to_node(
            &self,
            _repository: Arc<RepositoryContext>,
            _node: &Node,
            _path: &RelativePath,
            _file_size: u64,
            _content: &lore_storage::ContentHashMemo<'_>,
        ) -> Result<NodeComparison, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn make_executable(
            &self,
            _path: FilesystemPath<'_>,
            _executable: bool,
        ) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn create_dir_all(&self, _path: FilesystemPath<'_>) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn create_file(&self, _path: FilesystemPath<'_>) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn unify_case_rename(
            &self,
            _from: FilesystemPath<'_>,
            _to: FilesystemPath<'_>,
        ) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn remove(&self, _path: FilesystemPath<'_>) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn remove_recursive(&self, _path: FilesystemPath<'_>) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn set_file_to_immutable_store_contents(
            &self,
            _repository: Arc<RepositoryContext>,
            _node: &Node,
            _path: FilesystemPath<'_>,
        ) -> Result<(Fragment, Option<FileInfo>), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn copy_to_scratch_file(
            &self,
            _source_path: FilesystemPath<'_>,
            _destination_path: impl AsRef<Path>,
        ) -> Result<(), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn merge3_text_by_path(
            &self,
            _base: &RelativePath,
            _mine: &RelativePath,
            _theirs: &RelativePath,
            _result: &RelativePath,
            _mode: MergeTextMode<'_>,
        ) -> Result<bool, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn infer_is_diffable(&self, _path: FilesystemPath<'_>) -> Result<bool, FsError> {
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
        let link = Arc::new(parent.to_link_context(RepositoryId::from([1; 16])).await);

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

        operation.finalize(true).await.expect("Finalize failed");

        assert_eq!(1, filesystem.begins());
        assert_eq!(vec![true], *(filesystem.finalize_events.lock()));
    }

    #[tokio::test]
    async fn a_failing_operation_is_still_finalized() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), false, async |_operation| {
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

        let value: u32 = with_operation(repository.file_system(), true, async |_operation| {
            Ok::<_, FsError>(7)
        })
        .await
        .expect("The work succeeded");

        assert_eq!(7, value);
        assert_eq!(1, filesystem.begins());
        assert_eq!(vec![true], *(filesystem.finalize_events.lock()));
    }

    #[tokio::test]
    async fn a_finalize_failure_is_reported_where_the_work_succeeded() {
        let filesystem = Arc::new(TestFilesystemProvider::failing_finalize());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), false, async |_operation| Ok(())).await;

        result.expect_err("A finalize failure should be reported");
    }

    #[tokio::test]
    async fn the_works_error_is_reported_ahead_of_a_finalize_failure() {
        let filesystem = Arc::new(TestFilesystemProvider::failing_finalize());
        let repository = test_repository(filesystem.clone()).await;

        let result: Result<(), FsError> =
            with_operation(repository.file_system(), false, async |_operation| {
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
                let mut changes = Vec::new();

                let stats = crate::state::diff_filesystem(
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
                    &mut changes,
                )
                .await
                .expect("An excluded path is not an error");

                assert!(changes.is_empty(), "An excluded path reported changes");
                assert_eq!(0, stats.file_add.load(std::sync::atomic::Ordering::Relaxed));
            })
            .await;
    }

    #[tokio::test]
    async fn finalizing_twice_is_refused() {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let repository = test_repository(filesystem.clone()).await;

        let operation = repository.file_system().begin_operation().await.unwrap();
        operation.finalize(true).await.expect("Finalize failed");
        operation
            .finalize(true)
            .await
            .expect_err("A second finalize should be refused");

        assert_eq!(
            vec![true],
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
