use std::ffi::CString;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::*;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_error_set::ForwardAny;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use parking_lot::RwLock;

use crate::filter;
use crate::filter::Filter;
use crate::filter::FilterInstance;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::invalid_filesystem_provider::InvalidFilesystemProvider;
use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
use crate::fs::swfs::callback_counters::CallbackCounters;
use crate::fs::swfs::file::SwfsFile;
use crate::fs::swfs::file::SwfsFileArray;
use crate::fs::swfs::modified_tree::ModifiedTree;
use crate::fs::swfs::paths::MountPath;
use crate::fs::swfs::paths::SwfsPath;
use crate::fs::swfs::served_tree::Served;
use crate::fs::swfs::served_tree::ServedTree;
use crate::global::external_dir::check_for_external_lore_dir;
use crate::global::external_dir::external_dir_for_instance;
use crate::immutable;
use crate::instance::InstanceId;
use crate::instance::load_current_anchor;
use crate::instance::load_staged_revision;
use crate::interface::ExecutionContext;
use crate::interface::LoreGlobalArgs;
use crate::layer;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::relay::EventDispatcher;
use crate::repository::DOT_LORE;
use crate::repository::DOT_LOREIGNORE;
use crate::repository::ID;
use crate::repository::RemoteState;
use crate::repository::RepositoryConfig;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryPaths;
use crate::repository::VIEW_FILTER;
use crate::repository::VfsType;
use crate::repository::claim_opened_stores;
use crate::repository::create_client_immutable_store;
use crate::repository::create_client_mutable_store;
use crate::repository::load_repository_config_from_dot_dir;
use crate::repository::read_id_from_file;
use crate::state;
use crate::state::State;
use crate::state::StateError;
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

#[derive(Default)]
struct InternalState {
    /// What the mount serves, loaded with the instance's state at each thaw. `None` before the
    /// first load.
    served: Option<Arc<ServedTree>>,
}

impl std::fmt::Debug for InternalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalState")
            .field(
                "current",
                &self
                    .served
                    .as_ref()
                    .and_then(|served| served.current().map(|state| state.revision())),
            )
            .finish()
    }
}

/// Everything that defines a SWFS-mounted repository, excluding the result of the actual mount
/// itself.
#[derive(Debug)]
pub struct MountResources {
    global_args: LoreGlobalArgs,
    repository: Arc<RepositoryContext>,
    internal_state: RwLock<InternalState>,
    /// The files SWFS has reported as modified. Fed by the `notify_*` callbacks.
    modified: Arc<ModifiedTree>,
    /// How often SWFS has called back for content and for directories.
    callbacks: CallbackCounters,
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

    /// [`premount_instance_with_config`](Self::premount_instance_with_config) for an instance
    /// just created. Nothing has written to it, so the record of what is modified holds every
    /// change there is, and is trusted from the start. An instance mounted again starts with the
    /// record untrusted, and its first operation rebuilds it from SWFS's own list of what it holds
    /// modified, which SWFS keeps across a remount.
    pub async fn premount_new_instance(
        repository_id: RepositoryId,
        instance_id: InstanceId,
        mount_path: MountPath,
        config: &RepositoryConfig,
    ) -> Result<MountResources, SwfsMountError> {
        let resources =
            Self::premount_instance_with_config(repository_id, instance_id, mount_path, config)
                .await?;
        resources.modified.set_trusted(true);
        Ok(resources)
    }

    pub async fn premount_instance_with_config(
        repository_id: RepositoryId,
        instance_id: InstanceId,
        mount_path: MountPath,
        config: &RepositoryConfig,
    ) -> Result<MountResources, SwfsMountError> {
        if config.vfs_type() != VfsType::Swfs {
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

        let (immutable_store, opening) = create_client_immutable_store(
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

        // A mount uses its stores for as long as it is mounted, so it claims them for that
        // long, as a command does for its duration; the operations it serves then take
        // nothing of their own.
        let (immutable_store, mutable_store, stores) =
            claim_opened_stores(immutable_store, opening, |immutable_store| {
                create_client_mutable_store(config, &dot_directory, immutable_store)
            })
            .await
            .forward_any::<SwfsMountError>("Creating and claiming the stores for the mount")?;

        let name = hex::encode(instance_id.data());

        let mutable_store: Arc<dyn lore_storage::MutableStore> = mutable_store;
        let repository = Arc::new(
            RepositoryContext::new_with_state(
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
            )
            .with_store_hold(stores),
        );
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
            modified: Arc::new(ModifiedTree::new(repository.clone())),
            callbacks: CallbackCounters::default(),
            repository,
            internal_state: RwLock::default(),
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

    pub fn repository(&self) -> Arc<RepositoryContext> {
        self.repository.clone()
    }

    /// The parent's current revision, which the mount serves outside its links and layers.
    pub fn current_read_state(&self) -> Option<Arc<State>> {
        self.internal_state
            .read()
            .served
            .as_ref()
            .and_then(|served| served.current().cloned())
    }

    /// What the mount serves at each path of the working tree (ADR-00036).
    pub fn served_tree(&self) -> Arc<ServedTree> {
        self.internal_state
            .read()
            .served
            .clone()
            .unwrap_or_else(|| Arc::new(ServedTree::new(self.repository.clone(), None, Vec::new())))
    }

    pub fn modified(&self) -> &Arc<ModifiedTree> {
        &self.modified
    }

    pub fn callbacks(&self) -> &CallbackCounters {
        &self.callbacks
    }

    /// Loads what the mount serves from what the instance has stored: the current revision, the
    /// links at the pins the staged state holds, and the layers at their current revisions.
    pub async fn update_state(&self, token: SwfsExecutionToken) -> Result<(), SwfsWorkError> {
        let current_anchor = load_current_anchor(&self.repository).await;
        let current_revision = current_anchor.ok().map(|(revision, _)| revision);
        let current = match current_revision {
            Some(current_revision) => {
                self.modified.observe_revision(current_revision).await;
                Some(
                    state::State::deserialize(self.repository.clone(), current_revision)
                        .await
                        .forward::<SwfsWorkError>("Failed to deserialize state")?,
                )
            }
            None => None,
        };
        let staged = match load_staged_revision(&self.repository).await {
            Ok(Some(staged_revision)) if Some(staged_revision) != current_revision => Some(
                state::State::deserialize(self.repository.clone(), staged_revision)
                    .await
                    .forward::<SwfsWorkError>("Failed to deserialize the staged state")?,
            ),
            _ => None,
        };
        // An instance with no layer configuration has no layers.
        let layers = layer::list(self.repository.clone())
            .await
            .unwrap_or_default();
        let served = Arc::new(
            ServedTree::load(self.repository.clone(), current, staged, &layers)
                .await
                .forward::<SwfsWorkError>("Loading what the mount serves")?,
        );
        self.internal_state.write().served = Some(served);
        self.update_ignore_filter(token).await;
        Ok(())
    }

    /// Points the modified-file record at the ignore rules the current revision tracks. The
    /// mount path is virtual, so they are read from the immutable store rather than from disk.
    ///
    /// Rules that cannot be read leave only the exclusions every repository carries, so the
    /// record takes in more than it should rather than less.
    async fn update_ignore_filter(&self, token: SwfsExecutionToken) {
        let filter = match self.load_ignore_filter(token).await {
            Ok(filter) => filter,
            Err(error) => {
                crate::lore_warn!(
                    "Unable to load the ignore rules for SWFS change tracking: {error}"
                );
                filter::from_instances(FilterInstance::default(), FilterInstance::default())
                    .unwrap_or_default()
            }
        };
        self.modified.set_ignore(Arc::new(filter));
    }

    async fn load_ignore_filter(&self, token: SwfsExecutionToken) -> Result<Filter, SwfsWorkError> {
        let ignore = match self.read_ignore_rules(token).await? {
            Some(bytes) => filter::parse_filter(&bytes, Path::new(DOT_LOREIGNORE))
                .forward::<SwfsWorkError>("Parsing the ignore rules")?,
            None => FilterInstance::default(),
        };
        let view = match self.repository.dot_dir_path() {
            Ok(dot_dir) => filter::load_filter(dot_dir.join(VIEW_FILTER))
                .forward::<SwfsWorkError>("Loading the view filter")?,
            Err(_) => FilterInstance::default(),
        };
        filter::from_instances(ignore, view).forward::<SwfsWorkError>("Building the filter")
    }

    async fn read_ignore_rules(
        &self,
        token: SwfsExecutionToken,
    ) -> Result<Option<Vec<u8>>, SwfsWorkError> {
        let Some(state) = self.current_read_state() else {
            return Ok(None);
        };
        let node = match state
            .find_node(self.repository.clone(), DOT_LOREIGNORE)
            .await
        {
            Ok(node) => node,
            Err(StateError::NodeNotFound(_)) => return Ok(None),
            Err(error) => return Err(error.forward::<SwfsWorkError>("Finding the ignore rules")),
        };
        let mut bytes = vec![0; node.size as usize];
        let range = 0..bytes.len();
        self.read_file(token, SwfsPath(DOT_LOREIGNORE), range, &mut bytes)
            .await?;
        Ok(Some(bytes))
    }

    /// What the mount serves at `path`: what the served tree resolves it to.
    async fn served(&self, path: &RelativePath) -> Result<Served, SwfsWorkError> {
        let served = self
            .served_tree()
            .resolve(path)
            .await
            .forward::<SwfsWorkError>("Resolving a served path")?
            .ok_or_else(|| SwfsWorkError::internal(format!("{path} is not served")))?;
        Ok(served)
    }

    pub async fn get_file_info(
        &self,
        _token: SwfsExecutionToken,
        path: &RelativePath,
    ) -> Result<FileInfo, SwfsWorkError> {
        Ok(self.served(path).await?.file_info(path).await)
    }

    /// Reads `file_range` of the file at `swfs_path` into `output_bytes`, from the store of the
    /// repository that serves it.
    pub async fn read_file(
        &self,
        _token: SwfsExecutionToken,
        swfs_path: SwfsPath<'_>,
        file_range: Range<usize>,
        output_bytes: &mut [u8],
    ) -> Result<(usize, FileInfo), SwfsWorkError> {
        let node_path = swfs_path.node_path()?;
        crate::lore_trace!(
            "SWFS MountResources::read_file: swfs_path={:?}, node_path={:?}, range={:?}",
            swfs_path.0,
            node_path.0.as_str(),
            file_range
        );
        let (repository, file_node) = match self.served(&node_path.0).await {
            Ok(Served::Node {
                repository, node, ..
            }) if !node.is_directory() => (repository, node),
            _ if swfs_path.is_root() => return Ok((0, FileInfo::Directory)),
            Ok(_) => {
                return Err(SwfsWorkError::internal(format!(
                    "{} is not a file",
                    node_path.0
                )));
            }
            Err(error) => return Err(error),
        };
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
            // Each repository is a partition of its own, so linked and layered content is read
            // through the context of the repository that serves it.
            let read_options = immutable::read_options_from_repository(&repository);
            immutable::read_into(
                repository.clone(),
                file_node.address,
                Some(clamped_range),
                &mut output_bytes[..read_size],
                read_options,
            )
            .await
            .forward::<SwfsWorkError>("Reading immutable store into buffer")?;
        }
        let mtime = file_modified_time(repository, &node_path.0).await;
        let file_info = FileInfo::from_node_and_mtime(&file_node, mtime);
        Ok((read_size, file_info))
    }

    /// The entries of the directory at `swfs_path`, as the served tree lists it: a link or layer
    /// as the directory it mounts.
    pub async fn enumerate_directory(
        &self,
        _token: SwfsExecutionToken,
        swfs_path: SwfsPath<'_>,
    ) -> Result<SwfsFileArray, SwfsWorkError> {
        let node_path = swfs_path.node_path()?;
        let served = self.served_tree();
        // With no revision the mount serves nothing of its own, and what an operation wrote, such
        // as a clone's first files, is in SWFS's write cache: every directory, the root included,
        // lists as holding nothing more.
        if served.current().is_none() {
            return Ok(SwfsFileArray::empty());
        }
        // SWFS asks to list only what it was given as a directory, but a path that is none is
        // refused rather than listed: a callback must fail rather than panic.
        let Some(children) = served
            .children(&node_path.0)
            .await
            .forward::<SwfsWorkError>("Listing a served directory")?
        else {
            return Err(SwfsWorkError::internal(format!(
                "{} is not a served directory",
                node_path.0
            )));
        };

        let mut files = Vec::new();
        for child in children {
            let child_path = node_path.0.join(&child.name);
            let file_info = child.served.file_info(&child_path).await;
            files.push(
                SwfsFile::new(
                    CString::new(child.name).internal("Making CString from file name")?,
                    &file_info,
                    None,
                )
                .forward::<SwfsWorkError>("Constructing file info")?,
            );
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
    #[lore_macro::test_pub]
    fn new() -> Self {
        Self {}
    }
}

#[cfg(feature = "test-util")]
pub mod test_util {
    use super::*;

    impl MountResources {
        /// Mount resources serving `state` of `repository` at `mount_path`, with nothing mounted:
        /// what the callbacks answer, without SWFS or an instance directory.
        pub fn for_test(
            repository: Arc<RepositoryContext>,
            state: Option<Arc<State>>,
            mount_path: MountPath,
        ) -> Self {
            let resources = MountResources {
                global_args: LoreGlobalArgs::default(),
                modified: Arc::new(ModifiedTree::new(repository.clone())),
                callbacks: CallbackCounters::default(),
                repository,
                internal_state: RwLock::default(),
                _instance_id: InstanceId::default(),
                name_nul_terminated: CString::default(),
                _name: String::new(),
                write_path_nul_terminated: CString::default(),
                _write_path: PathBuf::new(),
                mount_path_nul_terminated: CString::default(),
                _mount_path: mount_path,
            };
            resources.internal_state.write().served = Some(Arc::new(ServedTree::new(
                resources.repository.clone(),
                state,
                Vec::new(),
            )));
            resources
        }
    }

    impl MountResources {
        /// Serves `served` in place of what the instance has stored.
        pub fn serve_for_test(&self, served: ServedTree) {
            self.internal_state.write().served = Some(Arc::new(served));
        }
    }

    impl SwfsExecutionToken {
        /// A token for a test calling what a callback would, which holds no runtime of its own.
        pub fn for_test() -> Self {
            Self::new()
        }
    }
}
