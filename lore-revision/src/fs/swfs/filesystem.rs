use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;

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
use crate::immutable;
use crate::node::Node;
use crate::repository::RepositoryContext;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
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
        self.mount.freeze().internal("Freezing SWFS file system")?;
        Ok(SwfsOperation {
            mount: self.mount.clone(),
            _repo_path: self.repo_path.clone(),
            os: OsFilesystem::begin_operation(&self.os),
        })
    }
}

#[async_trait]
impl FilesystemProvider for SwfsFilesystem {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Swfs(self.begin_operation().await?),
        )))
    }
}

/// SWFS-backed filesystem operation context.
pub struct SwfsOperation {
    mount: Arc<SwfsInterface>,
    _repo_path: PathBuf,
    os: OsOperation,
}

impl SwfsOperation {
    /// Thaws the frozen filesystem, clearing the write cache where the operation wrote.
    pub(crate) async fn finalize(&self, changes_made: bool) -> Result<(), FsError> {
        let token = SwfsExecutionToken {};
        self.mount
            .thaw(token, changes_made)
            .await
            .internal("Thawing SWFS file system")?;
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
        Ok(self
            .mount
            .resources()
            .get_file_info(SwfsExecutionToken {}, path)
            .await
            .internal("Getting node's file info")?)
    }

    async fn make_executable(
        &self,
        _path: &RelativePath,
        _executable: bool,
    ) -> Result<(), FsError> {
        Ok(())
    }

    async fn create_dir_all(&self, _path: &RelativePath) -> Result<(), FsError> {
        Ok(())
    }

    fake_with_os!(write_file, (), path: &RelativePath, contents: Bytes);

    async fn remove_recursive(&self, _path: &RelativePath) -> Result<(), FsError> {
        Ok(())
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        _path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        let mut buffer = vec![0; node.size as usize];
        let options = immutable::read_options_from_repository(&repository);
        immutable::read_into(repository, node.address, None, &mut buffer, options)
            .await
            .internal("Failed to cache file contents")?;
        Ok((Fragment::default(), None))
    }

    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        self.os.changes_from_filesystem_to_state(diff)
    }

    fake_with_os!(rename, (),
        _from: &RelativePath,
        _to: &RelativePath,
    );

    fake_with_os!(remove, (), _path: &RelativePath,);

    // A file the revision does not track is written to disk rather than to the mount, which
    // holds no node to answer for it.
    fake_with_os!(untracked_file_info, FileInfo, _path: &RelativePath,);

    fake_with_os!(copy_file, (),
        _source_path: &RelativePath,
        _destination_path: &RelativePath,
    );

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        self.os.holds_name_exactly(path).await
    }

    fake_with_os!(read_directory, DirectoryListing, _path: &RelativePath,);

    fake_with_os!(write_node, FileInfo,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath);

    fn content_source(&self, path: &RelativePath) -> lore_storage::ContentSource<'static> {
        self.os.content_source(path)
    }

    fake_with_os!( file_holds_content, NodeComparison,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes);
}
