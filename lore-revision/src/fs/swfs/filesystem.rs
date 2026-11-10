use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashSet;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;

use crate::fs::filesystem_provider::CloneAnswer;
use crate::fs::filesystem_provider::DirectoryListing;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
use crate::fs::os::OsFilesystem;
use crate::fs::os::OsOperation;
use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::mount_resources::SwfsExecutionToken;
use crate::fs::swfs::paths::RelativePathKey;
use crate::fs::swfs::state::tracked_file_info;
use crate::immutable;
use crate::node::Node;
use crate::repository::RepositoryContext;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
use crate::state::StateError;
use crate::util::path::RelativePath;

#[error_set]
pub enum SwfsFilesystemError {}

/// SWFS-backed filesystem provider.
#[derive(Debug)]
pub struct SwfsFilesystem {
    repo_path: PathBuf,
    mount: Arc<SwfsInterface>,
    os: OsFilesystem,
}

impl SwfsFilesystem {
    /// Create a new SWFS-backed filesystem provider.
    pub fn new(repo_path: impl AsRef<Path>, mount: Arc<SwfsInterface>) -> Self {
        Self {
            repo_path: repo_path.as_ref().to_path_buf(),
            mount,
            os: OsFilesystem::new(repo_path),
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn begin_operation(&self) -> Result<SwfsOperation, FsError> {
        self.mount
            .freeze(SwfsExecutionToken {})
            .await
            .internal("Freezing SWFS file system")?;
        Ok(SwfsOperation {
            mount: self.mount.clone(),
            _repo_path: self.repo_path.clone(),
            touched_nodes: Default::default(),
            os: OsFilesystem::begin_operation(&self.os),
        })
    }
}

#[async_trait]
impl FilesystemProvider for SwfsFilesystem {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        let holds_state = self.holds_state();
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Swfs(self.begin_operation().await?),
            holds_state,
        )))
    }

    async fn clone_answer(
        &self,
        _source: &Path,
        _dot_path: &Path,
        _destination: &Path,
    ) -> Result<CloneAnswer, crate::fs::filesystem_provider::InstanceCloneError> {
        Ok(CloneAnswer::Unsupported {
            reason: "this virtual filesystem provider can't clone instances".to_string(),
        })
    }
}

/// SWFS-backed filesystem operation context.
pub struct SwfsOperation {
    mount: Arc<SwfsInterface>,
    _repo_path: PathBuf,
    /// Paths this operation has written or removed, for thaw to describe to SWFS. Whether a path
    /// was added or deleted makes no difference to that, so they share the set.
    touched_nodes: DashSet<RelativePathKey>,
    os: OsOperation,
}

impl SwfsOperation {
    /// Notes that this operation wrote or removed `path`, so thaw describes it to SWFS. The root is
    /// never written as such.
    fn record_written(&self, path: &RelativePath) {
        if !path.is_empty() {
            self.touched_nodes.insert(RelativePathKey(path.clone()));
        }
    }

    /// Thaws the frozen filesystem, clearing the write cache where the operation wrote.
    pub(crate) async fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
        let token = SwfsExecutionToken {};
        let result = self
            .mount
            .thaw(token, changes_made, &self.touched_nodes)
            .await;
        // Whatever the outcome, none of these writes are pending any longer.
        self.touched_nodes.clear();
        result.internal("Thawing SWFS file system")?;
        Ok(())
    }
}

macro_rules! fake_with_os {
    ($fn_name:ident, $result_ty:ty, $($arg_name:ident: $arg_ty:ty),* $(,)?) => {
        async fn $fn_name(&self, $($arg_name: $arg_ty),*) -> Result<$result_ty, FsError> {
            self.os.$fn_name($($arg_name),*).await
        }
    };
}

impl InstanceOperation for SwfsOperation {
    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        let resources = self.mount.resources();
        let Some(state) = resources.current_read_state() else {
            return Ok(if path.is_empty() {
                FileInfo::Directory
            } else {
                FileInfo::NotExist
            });
        };
        let info = tracked_file_info(resources.repository(), &state, path)
            .await
            .internal("Unable to find file node")?;
        Ok(info)
    }

    async fn make_executable(&self, path: &RelativePath, _executable: bool) -> Result<(), FsError> {
        self.record_written(path);
        Ok(())
    }

    async fn create_dir_all(&self, path: &RelativePath) -> Result<(), FsError> {
        self.record_written(path);
        Ok(())
    }

    async fn write_file(&self, path: &RelativePath, _contents: Bytes) -> Result<(), FsError> {
        self.record_written(path);
        Ok(())
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        self.record_written(path);
        Ok(())
    }

    /// Removes `path` by naming it as holding nothing in the thaw, which drops it. A removal
    /// through the mount would be the user's own deletion, which SWFS keeps over every later
    /// thaw, so a later operation could not write the path again. A path the mount holds
    /// nothing at goes to the OS, since naming it would make SWFS create an empty file there.
    async fn remove(&self, path: &RelativePath) -> Result<(), FsError> {
        if !self.os.untracked_file_info(path).await?.exists() {
            return self.os.remove(path).await;
        }
        self.record_written(path);
        Ok(())
    }

    async fn remove_empty_directory(&self, path: &RelativePath) -> Result<(), FsError> {
        self.os.remove_empty_directory(path).await?;
        self.record_written(path);
        Ok(())
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        self.os.rename(from, to).await?;
        self.record_written(from);
        self.record_written(to);
        Ok(())
    }

    /// Both resolve to one node of the served State, which finds a child by any spelling of its
    /// name, compared by its whole link. A path the State holds no node at names nothing, as does
    /// every path before the mount serves a State.
    async fn names_one_entry(
        &self,
        from: &RelativePath,
        to: &RelativePath,
    ) -> Result<Option<bool>, FsError> {
        let resources = self.mount.resources();
        let Some(state) = resources.current_read_state() else {
            return Ok(None);
        };
        let repository = resources.repository();
        let node_at = async |path: &RelativePath| match state
            .find_node_link(repository.clone(), path.as_str())
            .await
        {
            Ok(link) if link.is_valid() => Ok(Some((link.repository, link.node, link.revision))),
            Ok(_) | Err(StateError::NodeNotFound(_)) => Ok(None),
            Err(err) => Err(FsError::internal_with_context(
                err,
                "finding the node a path names",
            )),
        };
        let Some(from) = node_at(from).await? else {
            return Ok(None);
        };
        Ok(Some(node_at(to).await? == Some(from)))
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        self.os.copy_file(source_path, destination_path).await?;
        self.record_written(destination_path);
        Ok(())
    }

    async fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<FileInfo, FsError> {
        let options = immutable::read_options_from_repository(&repository);
        immutable::read(repository, node.address, None, options)
            .await
            .internal("Failed to cache file contents")?;

        self.record_written(path);
        Ok(if node.is_directory() {
            FileInfo::Directory
        } else {
            FileInfo::from_node_and_mtime(node, 0)
        })
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        let options = immutable::read_options_from_repository(&repository);
        immutable::read(repository, address, None, options)
            .await
            .internal("Failed to cache file contents")?;

        self.record_written(path);
        Ok((Fragment::default(), None))
    }

    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        self.os.changes_from_filesystem_to_state(diff)
    }

    // A file the revision does not track is written to disk rather than to the mount, which
    // holds no node to answer for it.
    fake_with_os!(untracked_file_info, FileInfo, _path: &RelativePath,);

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        self.os.holds_name_exactly(path).await
    }

    fake_with_os!(read_directory, DirectoryListing, _path: &RelativePath,);

    fake_with_os!(content_source, lore_storage::ContentSource<'static>, path: &RelativePath);

    fake_with_os!(file_holds_content, NodeComparison,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes);
}
