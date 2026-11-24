use std::any::Any;
use std::ffi::CString;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashSet;
use lore_base::error::*;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::types::Hash;
use lore_error_set::ForwardAny;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;
use lore_storage::FlushOptions;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use parking_lot::RwLock;

use crate::filter;
use crate::filter::Filter;
use crate::filter::FilterInstance;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::invalid_filesystem_provider::InvalidFilesystemProvider;
use crate::fs::swfs::absent_paths::AbsentPaths;
use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
use crate::fs::swfs::callback_counters::CallbackCounters;
use crate::fs::swfs::file::SwfsFile;
use crate::fs::swfs::file::SwfsFileArray;
use crate::fs::swfs::modified_tree::ModifiedTree;
use crate::fs::swfs::paths::MountPath;
use crate::fs::swfs::paths::RelativePathKey;
use crate::fs::swfs::paths::SwfsPath;
use crate::fs::swfs::renamed_at_risk::RenamedAtRisk;
use crate::fs::swfs::served_tree::Served;
use crate::fs::swfs::served_tree::ServedTree;
use crate::immutable;
use crate::instance::InstanceId;
use crate::instance::load_current_anchor;
use crate::instance::load_staged_revision;
use crate::interface::LoreGlobalArgs;
use crate::layer;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
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
use crate::state::State;
use crate::state::StateError;
use crate::state::file_modified_time;
use crate::util::path::RelativePath;

pub const WRITE_DIRECTORY: &str = "write";

#[error_set]
pub enum SwfsMountError {}

#[error_set]
pub enum SwfsWorkError {
    Cancelled,
    LinkPinsDiverge,
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
    /// The execution context of the call that mounted the instance, which every callback runs in,
    /// so the mount reads that caller's configuration and credentials.
    context: Arc<dyn Any + Send + Sync>,
    repository: Arc<RepositoryContext>,
    /// Names the remote a failed connect is retried against.
    config: RepositoryConfig,
    internal_state: RwLock<InternalState>,
    /// What the mount last described to SWFS: the tree loaded when it was mounted, then the one
    /// each thaw that applied changes described. Links and layers that differ from it are
    /// described by their mount paths (ADR-00037), whatever moved them in between, such as a
    /// commit inside a link, which moves its pin without a thaw.
    described: RwLock<Option<Arc<ServedTree>>>,
    /// The files SWFS has reported as modified. Fed by the `notify_*` callbacks.
    modified: Arc<ModifiedTree>,
    /// How often SWFS has called back for content and for directories.
    callbacks: CallbackCounters,
    /// The moves SWFS has reported, and the renamed files to give bytes of their own: a
    /// workaround until SWFS names a renamed file's origin ([`RenamedAtRisk`]).
    renamed: Arc<RenamedAtRisk>,
    /// The ignore rules and the instance's view, of which the view decides what is served: a path
    /// it excludes is left out of listings and reads, as a plain instance never writes it
    /// (ADR-00034).
    served_filter: RwLock<Arc<Filter>>,
    /// The content of the user's own `.loreignore`, where SWFS holds it changed, which the
    /// instance's ignore rules are read from in place of the revision's: empty where the user
    /// deleted it. Set as each operation begins ([`Self::use_users_ignore_rules`]).
    users_ignore: RwLock<Option<Bytes>>,
    /// The ignore rules in effect: rules that change leave the modified-file record untrusted.
    ignore_rules: parking_lot::Mutex<RulesInEffect>,
    /// Lowercase paths the current revision holds that the working tree does not, left out of
    /// what is served: what an operation that moved the instance to the revision did not write
    /// (ADR-00034).
    absent: Arc<AbsentPaths>,
    /// The directories SWFS has listed through `fill_dir_begin` since the mount, as lowercase
    /// paths, the root as the empty one. See [`Self::list_directories_described_again`].
    listed: DashSet<String>,
    /// The directories a thaw described something in while SWFS had not listed them, as lowercase
    /// paths, until SWFS lists them. See [`Self::list_directories_described_again`].
    described_unlisted: DashSet<String>,
    /// Paths below a changed link or layer mount that an operation removed, which the next thaw
    /// describes as holding nothing although the state it loads still serves them: the operation
    /// realizes the change before the state that holds it is stored.
    removed_below_mounts: DashSet<String>,
    _instance_id: InstanceId,
    _name: String,
    pub name_nul_terminated: CString,
    _write_path: PathBuf,
    pub write_path_nul_terminated: CString,
    mount_path: MountPath,
    pub mount_path_nul_terminated: CString,
}

impl MountResources {
    /// The resources of a mount of the registered instance `instance_id`, whose external
    /// directory is `instance_directory`, at `mount_path`.
    pub async fn premount_instance(
        instance_id: InstanceId,
        mount_path: MountPath,
        instance_directory: &Path,
    ) -> Result<MountResources, SwfsMountError> {
        let dot_directory = instance_directory.join(DOT_LORE);
        let repository_config = load_repository_config_from_dot_dir(&dot_directory)
            .internal("Loading repository config")?;
        let repository_id =
            read_id_from_file(dot_directory.join(ID)).internal("Reading mounted repository ID")?;

        Self::premount_instance_with_config(
            repository_id,
            instance_id,
            mount_path,
            &repository_config,
            instance_directory,
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
        instance_directory: &Path,
    ) -> Result<MountResources, SwfsMountError> {
        let resources = Self::premount_instance_with_config(
            repository_id,
            instance_id,
            mount_path,
            config,
            instance_directory,
        )
        .await?;
        resources.modified.set_trusted(true);
        Ok(resources)
    }

    /// The resources of a mount at `mount_path` of the instance `instance_id` of the repository
    /// `repository_id`, with the config `config`, whose external directory is
    /// `instance_directory`.
    pub async fn premount_instance_with_config(
        repository_id: RepositoryId,
        instance_id: InstanceId,
        mount_path: MountPath,
        config: &RepositoryConfig,
        instance_directory: &Path,
    ) -> Result<MountResources, SwfsMountError> {
        if config.vfs_type() != VfsType::Swfs {
            return Err(SwfsMountError::internal(
                "Attempting to mount a non-SWFS instance",
            ));
        }

        let execution = execution_context();

        let write_directory = instance_directory.join(WRITE_DIRECTORY);
        lore_io::create_dir_in_existing_parent(&write_directory)
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

        let repo_path = mount_path
            .as_ref()
            .to_str()
            .ok_or(SwfsMountError::internal("Unable to stringify mount path"))?;
        let context: Arc<dyn Any + Send + Sync> = Arc::new(execution.serving(
            LoreGlobalArgs {
                repository_path: repo_path.into(),
                ..execution.globals().clone()
            },
            execution.environment().cloned(),
        ));
        let remote = LORE_CONTEXT.sync_scope(context.clone(), || {
            RemoteState::on_first_use(repository_id, config)
        });

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
                remote,
                Arc::new(Filter::default()),
                Some(Arc::new(InvalidFilesystemProvider {})),
            )
            .with_store_hold(stores),
        );
        repository.set_disable_cache(false);

        let resources = MountResources {
            context,
            modified: Arc::new(ModifiedTree::new(repository.clone())),
            callbacks: CallbackCounters::default(),
            renamed: Arc::default(),
            served_filter: RwLock::new(Arc::default()),
            users_ignore: RwLock::default(),
            ignore_rules: parking_lot::Mutex::default(),
            absent: AbsentPaths::load(repository.clone()).await,
            listed: DashSet::new(),
            described_unlisted: DashSet::new(),
            removed_below_mounts: DashSet::new(),
            repository,
            config: config.clone(),
            internal_state: RwLock::default(),
            described: RwLock::default(),
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
            mount_path,
        };
        resources
            .update_state(SwfsExecutionToken::new())
            .await
            .internal("Updating state before mounting")?;

        Ok(resources)
    }

    /// Runs the async callback in a Lore execution context, providing access to things like the
    /// global arguments. Call it from a thread that isn't a runtime worker: the thread blocks
    /// until the callback finishes, and runs the file operations the callback itself starts
    /// ([`lore_io::run_file_operations_on_this_thread`]) rather than waiting for a pool thread to.
    pub fn run_in_runtime_with_context<F: Future, RunFunc: FnOnce(SwfsExecutionToken) -> F>(
        &self,
        f: RunFunc,
    ) -> F::Output {
        let runtime = runtime();
        lore_io::run_file_operations_on_this_thread(|| {
            runtime
                .block_on(async move { self.run_with_context(f(SwfsExecutionToken::new())).await })
        })
    }

    pub async fn run_with_context<F: Future>(&self, f: F) -> F::Output {
        LORE_CONTEXT.scope(self.context.clone(), f).await
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

    /// What the mount last described to SWFS, `None` before the first load.
    pub fn described_tree(&self) -> Option<Arc<ServedTree>> {
        self.described.read().clone()
    }

    /// Notes that a thaw described `tree` to SWFS.
    pub fn set_described(&self, tree: Arc<ServedTree>) {
        *self.described.write() = Some(tree);
    }

    /// What the mount serves at each path of the working tree (ADR-00036).
    pub fn served_tree(&self) -> Arc<ServedTree> {
        self.internal_state
            .read()
            .served
            .clone()
            .unwrap_or_else(|| Arc::new(ServedTree::new(self.repository.clone(), None, Vec::new())))
    }

    /// Where the mount is, which SWFS serves from the stores this instance holds.
    pub fn mount_path(&self) -> &std::path::Path {
        self.mount_path.as_ref()
    }

    /// The ignore rules and the view the mount serves under.
    pub fn served_filter(&self) -> Arc<Filter> {
        self.served_filter.read().clone()
    }

    pub fn modified(&self) -> &Arc<ModifiedTree> {
        &self.modified
    }

    /// The moves SWFS has reported, and the renamed files to give bytes of their own.
    pub fn renamed(&self) -> &Arc<RenamedAtRisk> {
        &self.renamed
    }

    pub fn callbacks(&self) -> &CallbackCounters {
        &self.callbacks
    }

    /// Loads what the mount serves from what the instance has stored, and from the remote what the
    /// store lacks: the current revision, the links at the pins the staged state holds, and the
    /// layers at their current revisions. A remote whose connect failed is connected again. Runs
    /// in the mount's execution context, whichever command thaws it.
    ///
    /// The load is boxed: it runs once per command, and inline it would make the thaw's future as
    /// large as the load and the context scope together.
    pub async fn update_state(&self, token: SwfsExecutionToken) -> Result<(), SwfsWorkError> {
        self.run_with_context(Box::pin(self.load_served(token)))
            .await
    }

    async fn load_served(&self, token: SwfsExecutionToken) -> Result<(), SwfsWorkError> {
        self.repository.reconnect_if_lost(&self.config);
        let current_revision = load_current_anchor(&self.repository)
            .await
            .ok()
            .map(|(revision, _)| revision);
        let staged_revision = load_staged_revision(&self.repository)
            .await
            .ok()
            .flatten()
            .filter(|staged| Some(*staged) != current_revision);
        if let Some(current_revision) = current_revision {
            self.modified.observe_revision(current_revision).await;
        }
        let deserialize = |revision: Option<Hash>, failure: &'static str| async move {
            match revision {
                Some(revision) => State::deserialize_staged(
                    self.repository.clone(),
                    revision,
                    current_revision.unwrap_or_default(),
                )
                .await
                .map(Some)
                .forward::<SwfsWorkError>(failure),
                None => Ok(None),
            }
        };
        let (current, staged) = futures::try_join!(
            deserialize(current_revision, "Failed to deserialize state"),
            deserialize(staged_revision, "Failed to deserialize the staged state"),
        )?;
        // An instance with no layer configuration has no layers.
        let layers = layer::list(self.repository.clone())
            .await
            .unwrap_or_default();
        let served = Arc::new(
            ServedTree::load(self.repository.clone(), current, staged, &layers)
                .await
                .forward::<SwfsWorkError>("Loading what the mount serves")?,
        );
        self.internal_state.write().served = Some(served.clone());
        self.modified.set_served(served.clone());
        // The first load, at the mount, is what SWFS is serving from then on.
        self.described.write().get_or_insert(served);
        self.update_ignore_filter(token).await;
        Ok(())
    }

    /// Writes to disk what the mount fetched from the remote into its store. Unflushed, it is
    /// fetched again the next time the instance is mounted.
    pub async fn flush_fetched(&self) {
        if let Err(error) = self.repository.flush(FlushOptions::default()).await {
            crate::lore_warn!("SWFS mount could not flush its stores: {error}");
        }
    }

    /// Takes `users` as the content of the user's own `.loreignore`, where SWFS holds it changed,
    /// or `None` where it holds the revision's, and points the record at the rules that makes.
    pub async fn use_users_ignore_rules(&self, token: SwfsExecutionToken, users: Option<Bytes>) {
        *self.users_ignore.write() = users;
        self.update_ignore_filter(token).await;
    }

    /// Points the modified-file record at the instance's ignore rules: the user's own
    /// `.loreignore` where SWFS holds it changed, and otherwise the one the current revision
    /// tracks, read from the immutable store, since the mount path is virtual. Rules that differ
    /// from those in effect leave the record untrusted, for what it skipped was decided by them.
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
        let filter = Arc::new(filter);
        *self.served_filter.write() = filter.clone();
        self.modified.set_ignore(filter);
    }

    /// Whether the working tree leaves out `path`, which the current revision holds: the view
    /// excludes it and everything below it, or it is absent (ADR-00034).
    pub fn hides(&self, path: &RelativePath, is_directory: bool) -> bool {
        self.view_hides(path, is_directory) || self.is_absent(path)
    }

    /// Whether the view excludes `path` and everything below it.
    pub fn view_hides(&self, path: &RelativePath, is_directory: bool) -> bool {
        !path.is_empty()
            && self
                .served_filter
                .read()
                .excludes_tree(path, is_directory, FilterMode::View)
    }

    /// Whether the working tree lacks `path`, which the current revision holds (ADR-00034).
    pub fn is_absent(&self, path: &RelativePath) -> bool {
        !path.is_empty() && self.absent.contains(path)
    }

    /// Records `paths` as absent from the working tree, and `present` as no longer so, then
    /// persists the set. Called by an operation's finalize (ADR-00034).
    pub fn update_absent(&self, absent: &[RelativePath], present: &[RelativePath]) {
        self.absent.update(absent, present);
    }

    /// Lists through the mount each directory holding one of `written` that an earlier thaw
    /// described something in while SWFS had not listed it, shallowest first. Called while the
    /// mount is frozen, before the thaw that describes `written`, with the state it describes
    /// from loaded, which is what the listing answers from.
    ///
    /// A thaw naming a file in a directory SWFS has not listed makes SWFS an entry for it at the
    /// size described, and a later thaw naming it at another size leaves that size in place, even
    /// if the directory is listed after that thaw: a file shrunk by the later thaw reads as its new
    /// content padded with NUL bytes to the old size. Listing the directory before the later thaw
    /// keeps it in effect (the relist probe's `shrink` scenario). Only such a directory is listed,
    /// so a clone, whose thaw describes the whole tree, lists none of it through the mount. A
    /// directory that cannot be listed, such as one an earlier thaw described as removed, is left
    /// alone.
    pub async fn list_directories_described_again(&self, written: &DashSet<RelativePathKey>) {
        for directory in self.unlisted_directories(written) {
            if !self
                .described_unlisted
                .contains(&directory.to_ascii_lowercase())
            {
                continue;
            }
            let path = self.mount_path.as_ref().join(&directory);
            // Listed with a permit from the bound on syscalls on a mount (ADR-00035).
            let _permit = crate::fs::swfs::mount_io::MountIo::global().permit().await;
            match lore_io::IoDriver::global().read_dir(&path).await {
                Ok(mut entries) => while entries.next().await.is_some() {},
                Err(error) => {
                    crate::lore_debug!("SWFS: listing {} before a thaw: {error}", path.display());
                }
            }
        }
    }

    /// Notes each directory holding one of `written` that SWFS has not listed, which a later thaw
    /// describing something in it again lists first ([`Self::list_directories_described_again`]).
    /// Called after a thaw that described `written`.
    /// Notes that the thaw ending the operation describes `path` as holding nothing.
    pub fn note_removed_below_mount(&self, path: RelativePath) {
        self.removed_below_mounts.insert(path.as_str().to_owned());
    }

    /// The paths [`Self::note_removed_below_mount`] noted, which the thaw takes.
    pub fn take_removed_below_mounts(&self) -> DashSet<RelativePathKey> {
        let taken = DashSet::new();
        for path in self.removed_below_mounts.iter() {
            if let Ok(path) = RelativePath::new_from_initial_path(path.as_str()) {
                taken.insert(RelativePathKey(path));
            }
        }
        self.removed_below_mounts.clear();
        taken
    }

    pub fn note_described_unlisted(&self, written: &DashSet<RelativePathKey>) {
        for directory in self.unlisted_directories(written) {
            self.described_unlisted
                .insert(directory.to_ascii_lowercase());
        }
    }

    /// The directories holding one of `written` that SWFS has not listed since the mount,
    /// shallowest first, the root as the empty path.
    fn unlisted_directories(&self, written: &DashSet<RelativePathKey>) -> Vec<String> {
        let mut directories = std::collections::BTreeSet::new();
        for path in written.iter() {
            let mut directory = path.0.parent_path();
            loop {
                let text = directory.as_str();
                if !self.listed.contains(&text.to_ascii_lowercase()) {
                    let depth = if text.is_empty() {
                        0
                    } else {
                        text.matches('/').count() + 1
                    };
                    directories.insert((depth, text.to_owned()));
                }
                if directory.is_empty() {
                    break;
                }
                directory = directory.parent_path();
            }
        }
        directories
            .into_iter()
            .map(|(_, directory)| directory)
            .collect()
    }

    async fn load_ignore_filter(&self, token: SwfsExecutionToken) -> Result<Filter, SwfsWorkError> {
        let rules = self.read_ignore_rules(token).await?;
        let read = RulesInEffect::Read(rules.clone());
        let previous = std::mem::replace(&mut *self.ignore_rules.lock(), read.clone());
        let changed = previous != RulesInEffect::Unread && previous != read;
        if changed {
            self.modified.distrust();
        }
        let ignore = match rules {
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
        if let Some(users) = self.users_ignore.read().clone() {
            return Ok(Some(users.to_vec()));
        }
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

    /// What the mount serves at `path`: what the served tree resolves it to, unless the view
    /// excludes it or the working tree leaves it absent (ADR-00034).
    async fn served(&self, path: &RelativePath) -> Result<Served, SwfsWorkError> {
        let served = self
            .served_tree()
            .resolve(path)
            .await
            .forward::<SwfsWorkError>("Resolving a served path")?
            .ok_or_else(|| SwfsWorkError::internal(format!("{path} is not served")))?;
        if self.hides(path, served.is_directory()) {
            return Err(SwfsWorkError::internal("Node is not in the working tree"));
        }
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
        // lists as holding nothing more. None is recorded as listed, so the listing after the
        // thaw that first serves a revision is made: a directory never listed is how a later
        // thaw that shrinks a file in it pads the file with NUL bytes.
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
            if self.hides(&child_path, child.served.is_directory()) {
                continue;
            }
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
        let listed = node_path.0.as_str().to_ascii_lowercase();
        self.described_unlisted.remove(&listed);
        self.listed.insert(listed);

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

/// The ignore rules in effect for a mount, as read: none where neither the user nor the revision
/// holds a `.loreignore`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum RulesInEffect {
    /// Not read yet, as before the mount first loads its state.
    #[default]
    Unread,
    Read(Option<Vec<u8>>),
}

#[cfg(feature = "test-util")]
pub mod test_util {
    use super::*;

    impl MountResources {
        /// Mount resources serving `state` of `repository` at `mount_path`, with nothing mounted:
        /// what the callbacks answer, without SWFS or an instance directory.
        pub async fn for_test(
            repository: Arc<RepositoryContext>,
            state: Option<Arc<State>>,
            mount_path: MountPath,
        ) -> Self {
            let resources = MountResources {
                context: Arc::new(crate::interface::ExecutionContext::new_client(
                    LoreGlobalArgs::default(),
                    crate::relay::EventDispatcher::new(None),
                )),
                modified: Arc::new(ModifiedTree::new(repository.clone())),
                callbacks: CallbackCounters::default(),
                renamed: Arc::default(),
                served_filter: RwLock::new(Arc::default()),
                users_ignore: RwLock::default(),
                ignore_rules: parking_lot::Mutex::default(),
                absent: AbsentPaths::load(repository.clone()).await,
                listed: DashSet::new(),
                described_unlisted: DashSet::new(),
                removed_below_mounts: DashSet::new(),
                repository,
                config: RepositoryConfig::default(),
                internal_state: RwLock::default(),
                described: RwLock::default(),
                _instance_id: InstanceId::default(),
                name_nul_terminated: CString::default(),
                _name: String::new(),
                write_path_nul_terminated: CString::default(),
                _write_path: PathBuf::new(),
                mount_path_nul_terminated: CString::default(),
                mount_path,
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
        /// Whether SWFS has listed the directory at `path` since the mount, as far as
        /// [`Self::list_directories_described_again`] knows.
        pub fn listed_for_test(&self, path: &str) -> bool {
            self.listed.contains(&path.to_ascii_lowercase())
        }

        /// Whether a later thaw describing something in the directory at `path` lists it first.
        pub fn described_unlisted_for_test(&self, path: &str) -> bool {
            self.described_unlisted.contains(&path.to_ascii_lowercase())
        }

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
