// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Core filesystem provider traits for repository operations.
//!
//! This module defines the two-trait architecture that separates operation context creation
//! (freeze for SWFS) from actual file operations (work against frozen snapshot).

use std::fs::Metadata;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::error::InvalidArguments;
use lore_base::types::Fragment;
use lore_error_set::error_set;
use tokio::sync::RwLock;

use crate::change::NodeChange;
use crate::filter::FilterMode;
use crate::fs::os::OsOperation;
use crate::lore::Hash;
use crate::merge::MergeTextMode;
use crate::node::Node;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::FilesystemDiffStats;
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
    /// Whether the file is executable.
    pub executable: bool,
    /// File size in bytes (0 if doesn't exist or is directory).
    pub size: u64,
    /// Modification time as Unix timestamp in milliseconds.
    pub mtime: u64,
}

impl FileInfo {
    pub fn from_metadata(metadata: Metadata) -> Self {
        let (mtime, size) = crate::util::fs::file_mtime_and_size(&metadata);
        let executable = crate::util::fs::file_is_executable(&metadata);
        FileInfo {
            exists: true,
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            executable,
            size,
            mtime,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileDifferenceFromNode {
    /// Whether the file content differs from the node.
    pub modified: bool,
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
    /// This must not be called a second time until the first operation is finalized.
    ///
    /// # Implementation notes
    ///
    /// - **`OsFilesystem`**: Returns a lightweight wrapper with no state.
    /// - **`SWFS`**: Freezes the filesystem, creates a snapshot, returns operations that work
    ///   against the snapshot.
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError>;
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

type AssociatedOperation = (Arc<RepositoryContext>, Arc<InstanceOperationImpl>);

pub struct InstanceOperationImpl {
    dispatch: StaticDispatchInstanceOperation,
    associated_operations: RwLock<Option<Vec<AssociatedOperation>>>,
    modified_times: RecordedModifiedTimes,
}

impl InstanceOperationImpl {
    pub fn new(dispatch: StaticDispatchInstanceOperation) -> Self {
        Self {
            dispatch,
            associated_operations: RwLock::new(Some(Vec::new())),
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

    pub async fn associated_operation(
        &self,
        associated_repository: Arc<RepositoryContext>,
    ) -> Result<Arc<InstanceOperationImpl>, FsError> {
        let mut associated_operations = self.associated_operations.write().await;
        let Some(associated_operations) = associated_operations.as_mut() else {
            return Err(FsError::internal("Operation already finalized"));
        };
        for (repository, operation) in associated_operations.iter() {
            if Arc::ptr_eq(&associated_repository, repository) {
                return Ok(operation.clone());
            }
        }
        let new_operation = associated_repository
            .file_system()
            .begin_operation()
            .await?;
        associated_operations.push((associated_repository, new_operation.clone()));
        Ok(new_operation)
    }

    async fn recursively_finalize(&self, changes_made: bool) -> Result<(), FsError> {
        let mut associated_operations_option = self.associated_operations.write().await;
        let Some(associated_operations) = associated_operations_option.as_mut() else {
            return Err(FsError::internal("Operation already finalized"));
        };
        let mut result = self.dispatched_finalize(changes_made).await;
        for (_, associated_operation) in &mut *associated_operations {
            result =
                result.and(Box::pin(associated_operation.recursively_finalize(changes_made)).await);
        }
        *associated_operations_option = None;
        result
    }

    async fn dispatched_finalize(&self, changes_made: bool) -> Result<(), FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(this) => this.finalize(changes_made).await,
            StaticDispatchInstanceOperation::Os(this) => this.finalize(changes_made).await,
        }
    }
}

impl InstanceOperation for InstanceOperationImpl {
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
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
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
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => this.file_info(path).await,
        }
    }

    async fn is_file_modified(
        &self,
        repository: Arc<RepositoryContext>,
        node_change: &NodeChange,
        force_full_check: bool,
    ) -> Result<FileModifiedCheck, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.is_file_modified(repository, node_change, force_full_check)
                    .await
            }
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
    ) -> Result<NodeComparison, FsError> {
        match &self.dispatch {
            #[cfg(test)]
            StaticDispatchInstanceOperation::Test(_this) => panic!(),
            StaticDispatchInstanceOperation::Os(this) => {
                this.compare_file_to_node(repository, node, path, file_size)
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
        self.recursively_finalize(changes_made).await
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use async_trait::async_trait;
    use lore_base::types::Fragment;
    use parking_lot::Mutex;

    use crate::change::NodeChange;
    use crate::filter::FilterMode;
    use crate::fs::filesystem_provider::FileInfo;
    use crate::fs::filesystem_provider::FileModifiedCheck;
    use crate::fs::filesystem_provider::FilesystemPath;
    use crate::fs::filesystem_provider::FilesystemProvider;
    use crate::fs::filesystem_provider::FsError;
    use crate::fs::filesystem_provider::InstanceOperation;
    use crate::fs::filesystem_provider::InstanceOperationImpl;
    use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
    use crate::lore::Hash;
    use crate::merge::MergeTextMode;
    use crate::node::Node;
    use crate::node::NodeID;
    use crate::repository::RepositoryContext;
    use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
    use crate::repository::test_helpers::default_repository_creation_args;
    use crate::state::FilesystemDiffStats;
    use crate::state::NodeComparison;
    use crate::state::State;
    use crate::util::path::RelativePath;

    #[derive(Default)]
    pub struct TestFilesystemProvider {
        pub finalize_events: Arc<Mutex<Vec<bool>>>,
    }

    impl TestFilesystemProvider {
        pub fn new() -> TestFilesystemProvider {
            Self {
                finalize_events: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl FilesystemProvider for TestFilesystemProvider {
        async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
            Ok(Arc::new(InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Test(TestOperation {
                    finalize_events: self.finalize_events.clone(),
                }),
            )))
        }
    }

    pub struct TestOperation {
        finalize_events: Arc<Mutex<Vec<bool>>>,
    }

    impl InstanceOperation for TestOperation {
        /// The only actually implemented member, the rest are unimplemented which will fail any
        /// test that calls them.
        async fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
            self.finalize_events.lock().push(changes_made);
            Ok(())
        }

        async fn changes_from_filesystem_to_state(
            &self,
            _repository_from: Arc<RepositoryContext>,
            _state_from: Arc<State>,
            _repository_current: Arc<RepositoryContext>,
            _state_current: Arc<State>,
            _node_path: RelativePath,
            _root_node_from: NodeID,
            _root_node_to: NodeID,
            _filter_mode: FilterMode,
        ) -> Result<(Vec<NodeChange>, FilesystemDiffStats), FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn file_info(&self, _path: FilesystemPath<'_>) -> Result<FileInfo, FsError> {
            panic!("Test operation unimplemented except finalize")
        }

        async fn is_file_modified(
            &self,
            _repository: Arc<RepositoryContext>,
            _node_change: &NodeChange,
            _force_full_check: bool,
        ) -> Result<FileModifiedCheck, FsError> {
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
    async fn sub_repository_instance_operation_finalize() {
        async fn fake_repository() -> (Arc<TestFilesystemProvider>, Arc<RepositoryContext>) {
            let (immutable_store, mutable_store, _context) =
                test_store_create().await.expect("Making test stores");
            let provider = Arc::new(TestFilesystemProvider::new());
            (
                provider.clone(),
                Arc::new(RepositoryContext::new(
                    default_repository_creation_args(immutable_store, mutable_store)
                        .with_filesystem_provider(provider),
                )),
            )
        }

        let (parent_filesystem, parent_repo) = fake_repository().await;
        let (child_1_filesystem, child_1_repo) = fake_repository().await;
        let (child_2_filesystem, child_2_repo) = fake_repository().await;
        let (grandchild_filesystem, grandchild_repo) = fake_repository().await;

        let parent_operation = parent_repo.file_system().begin_operation().await.unwrap();
        let child_1_operation = parent_operation
            .associated_operation(child_1_repo)
            .await
            .unwrap();
        let _child_2_operation = parent_operation
            .associated_operation(child_2_repo)
            .await
            .unwrap();
        let _grandchild_operation = child_1_operation
            .associated_operation(grandchild_repo)
            .await;

        assert_eq!(
            Vec::<bool>::new(),
            *(parent_filesystem.finalize_events.lock())
        );
        assert_eq!(
            Vec::<bool>::new(),
            *(child_1_filesystem.finalize_events.lock())
        );
        assert_eq!(
            Vec::<bool>::new(),
            *(child_2_filesystem.finalize_events.lock())
        );
        assert_eq!(
            Vec::<bool>::new(),
            *(grandchild_filesystem.finalize_events.lock())
        );

        parent_operation
            .finalize(true)
            .await
            .expect("Finalize failed");

        assert_eq!(vec![true], *(parent_filesystem.finalize_events.lock()));
        assert_eq!(vec![true], *(child_1_filesystem.finalize_events.lock()));
        assert_eq!(vec![true], *(child_2_filesystem.finalize_events.lock()));
        assert_eq!(vec![true], *(grandchild_filesystem.finalize_events.lock()));

        parent_operation
            .finalize(true)
            .await
            .expect_err("Finalize should have failed");

        assert_eq!(vec![true], *(parent_filesystem.finalize_events.lock()));
        assert_eq!(vec![true], *(child_1_filesystem.finalize_events.lock()));
        assert_eq!(vec![true], *(child_2_filesystem.finalize_events.lock()));
        assert_eq!(vec![true], *(grandchild_filesystem.finalize_events.lock()));
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
