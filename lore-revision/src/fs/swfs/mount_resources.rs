use std::ffi::CString;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::*;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use parking_lot::RwLock;

use crate::filter::Filter;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::invalid_filesystem_provider::InvalidFilesystemProvider;
use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
use crate::fs::swfs::file::SwfsFile;
use crate::fs::swfs::file::SwfsFileArray;
use crate::fs::swfs::paths::MountPath;
use crate::fs::swfs::paths::SwfsPath;
use crate::global::external_dir::check_for_external_lore_dir;
use crate::global::external_dir::external_dir_for_instance;
use crate::immutable;
use crate::instance::InstanceId;
use crate::instance::load_current_anchor;
use crate::interface::ExecutionContext;
use crate::interface::LoreGlobalArgs;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::relay::EventDispatcher;
use crate::repository::DOT_LORE;
use crate::repository::ID;
use crate::repository::RemoteState;
use crate::repository::RepositoryConfig;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryPaths;
use crate::repository::create_client_immutable_store;
use crate::repository::create_client_mutable_store;
use crate::repository::load_repository_config_from_dot_dir;
use crate::repository::read_id_from_file;
use crate::state;
use crate::state::State;
use crate::state::file_modified_time;
use crate::util::path::RelativePath;

pub const WRITE_DIRECTORY: &str = "write";

#[error_set]
pub enum SwfsMountError {}

#[error_set]
pub enum SwfsWorkError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    RevisionNotFound,
    WriteRequired,
    Oversized,
    InvalidArguments,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    Disconnected,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    PayloadNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

/// Everything that defines a SWFS-mounted repository, excluding the result of the actual mount
/// itself.
#[derive(Debug)]
pub struct MountResources {
    global_args: LoreGlobalArgs,
    repository: Arc<RepositoryContext>,
    state: RwLock<Option<Arc<State>>>,
    _instance_id: InstanceId,
    _name: String,
    pub name_nul_terminated: CString,
    _write_path: PathBuf,
    pub write_path_nul_terminated: CString,
    _mount_path: MountPath,
    pub mount_path_nul_terminated: CString,
}

impl MountResources {
    pub async fn premount_instance(
        instance_id: InstanceId,
        mount_path: MountPath,
    ) -> Result<MountResources, SwfsMountError> {
        let dot_directory = check_for_external_lore_dir(instance_id)
            .forward::<SwfsMountError>("Finding instance directory")?
            .ok_or_else(|| {
                SwfsMountError::internal(format!(
                    "Missing instance directory at {}",
                    mount_path.as_ref().display()
                ))
            })?;

        let repository_config = load_repository_config_from_dot_dir(&dot_directory)
            .internal("Loading repository config")?;
        let repository_id =
            read_id_from_file(dot_directory.join(ID)).internal("Reading mounted repository ID")?;

        Self::premount_instance_with_config(
            repository_id,
            instance_id,
            mount_path,
            &repository_config,
        )
        .await
    }

    pub async fn premount_instance_with_config(
        repository_id: RepositoryId,
        instance_id: InstanceId,
        mount_path: MountPath,
        config: &RepositoryConfig,
    ) -> Result<MountResources, SwfsMountError> {
        if !config.is_swfs() {
            return Err(SwfsMountError::internal(
                "Attempting to mount a non-SWFS instance",
            ));
        }

        let execution = execution_context();

        let instance_directory = external_dir_for_instance(instance_id)
            .forward::<SwfsMountError>("Finding instance directory")?;
        let write_directory = instance_directory.join(WRITE_DIRECTORY);
        std::fs::create_dir_all(&write_directory)
            .internal("Creating write directory for SWFS mount")?;
        let dot_directory = instance_directory.join(DOT_LORE);

        let immutable_store = create_client_immutable_store(
            config,
            &dot_directory,
            config
                .store
                .as_ref()
                .map_or_else(ImmutableStoreCreateOptions::none, |store_config| {
                    store_config.to_options()
                }),
            config
                .store
                .as_ref()
                .and_then(|store_config| store_config.verify_write)
                .unwrap_or_default(),
        )
        .await
        .internal("Creating immutable store")?;

        let mutable_store =
            create_client_mutable_store(config, &dot_directory, immutable_store.clone())
                .await
                .internal("Creating mutable store")?;

        let name = hex::encode(instance_id.data());

        let repository = Arc::new(RepositoryContext::new_with_state(
            Some(RepositoryPaths::new(
                mount_path.as_ref().to_path_buf(),
                dot_directory,
            )),
            immutable_store,
            mutable_store,
            repository_id,
            instance_id,
            RemoteState::Offline,
            Arc::new(Filter::default()),
            Some(Arc::new(InvalidFilesystemProvider {})),
        ));
        repository.set_disable_cache(false);

        let repo_path = mount_path
            .as_ref()
            .to_str()
            .ok_or(SwfsMountError::internal("Unable to stringify mount path"))?;

        let resources = MountResources {
            global_args: LoreGlobalArgs {
                repository_path: repo_path.into(),
                ..execution.globals().clone()
            },
            repository,
            state: RwLock::new(None),
            _instance_id: instance_id,
            name_nul_terminated: CString::new(name.as_str())
                .internal("Nul terminating mount name")?,
            _name: name,
            write_path_nul_terminated: CString::new(write_directory.as_os_str().as_encoded_bytes())
                .internal("Nul terminating write path")?,
            _write_path: write_directory,
            mount_path_nul_terminated: mount_path
                .nul_terminated_swfs_form()
                .internal("Nul terminating mount path")?,
            _mount_path: mount_path,
        };
        resources
            .run_with_context(resources.update_state(SwfsExecutionToken::new()))
            .await
            .internal("Updating state before mounting")?;

        Ok(resources)
    }

    /// Runs the async callback in a Lore execution context, providing access to things like the
    /// global arguments.
    pub fn run_in_runtime_with_context<F: Future, RunFunc: FnOnce(SwfsExecutionToken) -> F>(
        &self,
        f: RunFunc,
    ) -> F::Output {
        let runtime = runtime();
        runtime.block_on(async move { self.run_with_context(f(SwfsExecutionToken::new())).await })
    }

    pub async fn run_with_context<F: Future>(&self, f: F) -> F::Output {
        let execution_context = Arc::new(ExecutionContext::new_client(
            self.global_args.clone(),
            // TODO: Provide a useful `EventDispatcher` to handle errors.
            EventDispatcher::new(None),
        ));

        LORE_CONTEXT.scope(execution_context, f).await
    }

    fn current_state(&self) -> Result<Option<Arc<State>>, SwfsWorkError> {
        Ok(self.state.read().clone())
    }

    pub async fn update_state(&self, _token: SwfsExecutionToken) -> Result<(), SwfsWorkError> {
        let current_anchor = load_current_anchor(&self.repository).await;
        *self.state.write() = match current_anchor {
            Ok((current_revision, _)) => Some(
                state::State::deserialize(self.repository.clone(), current_revision)
                    .await
                    .forward::<SwfsWorkError>("Failed to deserialize state")?,
            ),
            Err(_) => None,
        };
        Ok(())
    }

    pub async fn get_file_info(
        &self,
        _token: SwfsExecutionToken,
        path: &RelativePath,
    ) -> Result<FileInfo, SwfsWorkError> {
        let Some(state) = self.current_state()? else {
            return Ok(FileInfo::NotExist);
        };
        let node = state
            .find_node(self.repository.clone(), path.as_str())
            .await
            .internal("Unable to find file node")?;
        if node.is_directory() {
            Ok(FileInfo::Directory)
        } else {
            let mtime = file_modified_time(self.repository.clone(), path).await;
            Ok(FileInfo::from_node_and_mtime(&node, mtime))
        }
    }

    pub async fn read_file(
        &self,
        _token: SwfsExecutionToken,
        swfs_path: SwfsPath<'_>,
        file_range: Range<usize>,
        output_bytes: &mut [u8],
    ) -> Result<(usize, FileInfo), SwfsWorkError> {
        let Some(state) = self.current_state()? else {
            crate::lore_debug!("SWFS read_file: no state available");
            return Ok((0, FileInfo::NotExist));
        };
        let node_path = swfs_path.node_path()?;
        crate::lore_trace!(
            "SWFS MountResources::read_file: swfs_path={:?}, node_path={:?}, range={:?}",
            swfs_path.0,
            node_path.0.as_str(),
            file_range
        );
        let file_node = state
            .find_node(self.repository.clone(), node_path.0.as_str())
            .await
            .forward::<SwfsWorkError>("Finding node in state")?;
        crate::lore_trace!(
            "SWFS MountResources::read_file: found node with address={:?}, size={}",
            file_node.address,
            file_node.size
        );
        // Clamp the range to the actual file size - SWFS may request more than the file contains
        let file_size = file_node.size as usize;
        let clamped_start = file_range.start.min(file_size);
        let clamped_end = file_range.end.min(file_size);
        let clamped_range = clamped_start..clamped_end;
        let read_size = clamped_range.end - clamped_range.start;

        if read_size > 0 {
            let read_options = immutable::read_options_from_repository(&self.repository);
            immutable::read_into(
                self.repository.clone(),
                file_node.address,
                Some(clamped_range),
                &mut output_bytes[..read_size],
                read_options,
            )
            .await
            .forward::<SwfsWorkError>("Reading immutable store into buffer")?;
        }
        let mtime = file_modified_time(self.repository.clone(), &node_path.0).await;
        let file_info = FileInfo::from_node_and_mtime(&file_node, mtime);
        Ok((read_size, file_info))
    }

    pub async fn enumerate_directory(
        &self,
        _token: SwfsExecutionToken,
        swfs_path: SwfsPath<'_>,
    ) -> Result<SwfsFileArray, SwfsWorkError> {
        let Some(state) = self.current_state()? else {
            return Ok(SwfsFileArray::empty());
        };

        let node_path = swfs_path.node_path()?;
        let directory_node_id = if swfs_path.0 == "\\" {
            ROOT_NODE
        } else {
            state
                .find_node_link(self.repository.clone(), node_path.0.as_str())
                .await
                .forward::<SwfsWorkError>("Finding directory node ID in state")?
                .node
        };
        let directory_node = state
            .node(self.repository.clone(), directory_node_id)
            .await
            .forward::<SwfsWorkError>("Finding directory node in state")?;

        let mut files = Vec::new();

        let mut child_iter = directory_node.child();
        let mut cycle = SiblingCycleGuard::new(directory_node_id);
        while let Some(child_id) = child_iter {
            let child = state
                .node(self.repository.clone(), child_id)
                .await
                .forward::<SwfsWorkError>("Finding child node in state")?;
            let (child_node_name, child_node_path) = {
                let child_node_name = state
                    .node_name_ref(self.repository.clone(), child_id)
                    .await
                    .forward::<SwfsWorkError>("Finding child node name")?;
                let child_node_name: &str = child_node_name.as_ref();
                (
                    CString::new(child_node_name).internal("Making CString from file name")?,
                    node_path.join(child_node_name),
                )
            };
            let file_info = if child.is_directory() {
                FileInfo::Directory {}
            } else {
                let mtime = file_modified_time(self.repository.clone(), &child_node_path.0).await;
                FileInfo::from_node_and_mtime(&child, mtime)
            };

            files.push(
                SwfsFile::new(child_node_name, &file_info, None)
                    .forward::<SwfsWorkError>("Constructing file info")?,
            );

            child
                .walk_step(child_id, directory_node_id, &mut cycle)
                .forward::<SwfsWorkError>("Invalid node hierarchy in SWFS enumerate directory")?;
            child_iter = child.sibling();
        }

        Ok(SwfsFileArray::new(files))
    }
}

pub struct SwfsFileStruct(pub SWFSFile);
unsafe impl Send for SwfsFileStruct {}
unsafe impl Sync for SwfsFileStruct {}

/// Token that must be held whenever performing "regular" Lore operations that will assume an
/// `ExecutionContext` is available.
/// Created when called into from other parts of Lore, or when SWFS-driven callbacks jump to a tokio
/// task and an `ExecutionContext` is created for it.
#[derive(Debug, Clone, Copy)]
pub struct SwfsExecutionToken;

impl SwfsExecutionToken {
    fn new() -> Self {
        Self {}
    }
}
