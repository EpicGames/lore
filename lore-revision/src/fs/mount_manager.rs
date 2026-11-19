// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service's mounts. Each VFS has a manager of its own, which alone mounts, moves and
//! unmounts its instances: [`SwfsMountManager`] for SWFS and, on Linux, `FuseMountManager` for
//! FUSE. This one holds both, finds every registered instance once as the service starts and hands
//! each to its manager, and answers what is mounted at a path, whichever VFS mounted it.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::InvalidPath;
use lore_base::error::MountsBusy;
use lore_error_set::ForwardStrict;
use lore_error_set::error_set;

use crate::fs::filesystem_provider::FilesystemProvider;
#[cfg(target_os = "linux")]
use crate::fs::fuse::mount::FuseFilesystemProvider;
#[cfg(target_os = "linux")]
use crate::fs::fuse::mount_manager::FuseMountManager;
#[cfg(target_os = "linux")]
use crate::fs::fuse::mount_manager::remove_deleted_instance;
use crate::fs::swfs::filesystem::SwfsFilesystem;
use crate::fs::swfs::mount_manager::SwfsMount;
use crate::fs::swfs::mount_manager::SwfsMountManager;
use crate::fs::swfs::paths::MountPath;
use crate::global::external_dir::access::resolve_shared_in;
use crate::global::external_dir::canonical_global;
use crate::global::external_dir::resolve::Availability;
use crate::global::external_dir::resolve::Lookup;
use crate::global::external_dir::resolve::RootState;
use crate::global::external_dir::resolve::Unknown;
use crate::global::external_dir::resolve::resolve;
use crate::instance::InstanceId;
#[cfg(target_os = "linux")]
use crate::instance::InstanceMetadata;
use crate::instance::list_instances;
use crate::lore_warn;
use crate::repository::DOT_LORE;
#[cfg(target_os = "linux")]
use crate::repository::RepositoryConfig;
#[cfg(target_os = "linux")]
use crate::repository::RepositoryContext;
use crate::repository::VfsType;
use crate::repository::load_repository_config_from_dot_dir;
use crate::shared_store::registry::SharedStoreRegistry;
use crate::shared_store::registry::SharedStoreRegistryEntry;
use crate::util::config::SaveableConfig;

#[error_set]
pub enum MountManagerError {
    InvalidPath,
    MountsBusy,
}

/// What a read-write mount records of where its instance lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRecord {
    /// The instance's external directory.
    pub external_dir: PathBuf,
    /// The canonical path of the global data directory the external directory was resolved for.
    pub global: Arc<Path>,
}

/// A FUSE instance found at startup, mounted once the manager exists.
#[lore_macro::test_pub]
#[cfg(target_os = "linux")]
pub(crate) struct PendingFuseMount {
    pub(crate) repository: Arc<RepositoryContext>,
    pub(crate) instance: InstanceMetadata,
    pub(crate) dot_lore: PathBuf,
    pub(crate) config: RepositoryConfig,
    /// The canonical path of the global data directory the start resolved `dot_lore` for.
    pub(crate) global: Arc<Path>,
}

#[lore_macro::test_pub]
#[cfg(not(target_os = "linux"))]
struct PendingFuseMount;

/// The mounts a start made of its registered SWFS instances, not yet ready, by path.
type FoundSwfsMounts = Vec<(MountPath, SwfsMount)>;

/// The instances registered in the shared stores a start found, `None` where a store could not be
/// opened or listed, so some are unknown.
type RegisteredInstances = Option<BTreeSet<InstanceId>>;

pub struct MountManager {
    swfs: SwfsMountManager,
    #[cfg(target_os = "linux")]
    fuse: FuseMountManager,
}

impl MountManager {
    /// Mounts the SWFS instances of every store in `entries` and finds its FUSE instances, which
    /// are mounted once the manager exists, and the instances registered in them, in the external
    /// root `state` a start resolved once for them all. A store that cannot be opened or listed, and an
    /// instance whose external directory is unknown or whose config cannot be read, are logged
    /// and skipped rather than stopping the rest, and no external directory is removed for them.
    ///
    /// At the first virtual instance found, `state` is resolved again under shared access, and
    /// whether it was is returned. Where shared access is refused, no virtual instance is mounted,
    /// and where `state` reads a move of the root as interrupted, no store is opened either, so a
    /// start doesn't wait for a store the move waits for. An instance whose path `skip` answers for is
    /// left as it is.
    #[lore_macro::test_pub]
    async fn mounts_of_entries_skipping(
        entries: &[SharedStoreRegistryEntry],
        state: &mut RootState,
        skip: impl Fn(&Path) -> bool,
    ) -> Result<
        (
            FoundSwfsMounts,
            Vec<PendingFuseMount>,
            RegisteredInstances,
            bool,
        ),
        MountManagerError,
    > {
        if matches!(state.availability, Availability::Interrupted) {
            if !entries.is_empty() {
                lore_warn!(
                    "Not mounting any virtual instance: {}",
                    Unknown::Interrupted
                );
            }
            return Ok((Vec::new(), Vec::new(), None, false));
        }
        let mut swfs = Vec::new();
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut pending_fuse = Vec::new();
        let mut registered = Some(BTreeSet::new());
        let mut shared = false;
        let global = canonical_global(&state.global)
            .forward::<MountManagerError>("resolving the external root")?;
        for entry in entries {
            let entry_repository = match entry.create_null_repository_context().await {
                Ok(repository) => repository,
                Err(error) => {
                    lore_warn!("Unable to open shared store at {}: {error}", entry.path());
                    registered = None;
                    continue;
                }
            };
            let instances = match list_instances(&entry_repository).await {
                Ok(instances) => instances,
                Err(error) => {
                    lore_warn!(
                        "Unable to list instances of shared store at {}: {error}",
                        entry.path()
                    );
                    registered = None;
                    continue;
                }
            };
            if let Some(registered) = &mut registered {
                registered.extend(instances.iter().map(|instance| instance.instance_id));
            }
            for instance in &instances {
                if skip(Path::new(&instance.path)) {
                    continue;
                }
                let lookup = match lookup_under_shared(state, &mut shared, instance.instance_id) {
                    Ok(lookup) => lookup,
                    Err(error) => {
                        lore_warn!("Not mounting any virtual instance: {error}");
                        return Ok((Vec::new(), Vec::new(), None, false));
                    }
                };
                let dot_lore = match lookup {
                    Lookup::Present(dot_lore) => dot_lore,
                    Lookup::Absent => continue,
                    Lookup::Unknown(unknown) => {
                        lore_warn!(
                            "Not mounting the virtual instance at {}: {unknown}",
                            instance.path
                        );
                        continue;
                    }
                };
                let config = match load_repository_config_from_dot_dir(&dot_lore) {
                    Ok(config) => config,
                    Err(error) => {
                        lore_warn!(
                            "Unable to load the repository config of instance at {}: {error}",
                            instance.path
                        );
                        continue;
                    }
                };
                match config.vfs_type() {
                    #[cfg(target_os = "linux")]
                    VfsType::Fuse => {
                        if remove_deleted_instance(state, instance, &dot_lore).await {
                            continue;
                        }
                        pending_fuse.push(PendingFuseMount {
                            repository: entry_repository.clone(),
                            instance: InstanceMetadata {
                                instance_id: instance.instance_id,
                                path: instance.path.clone(),
                                created: instance.created,
                            },
                            dot_lore,
                            config,
                            global: global.clone(),
                        });
                    }
                    VfsType::Swfs => {
                        let mount_path =
                            MountPath::new(Path::new(&instance.path))
                                .forward::<MountManagerError>("Failed to make mount path")?;
                        let record = Arc::new(MountRecord {
                            external_dir: external_dir_of(&dot_lore)?,
                            global: global.clone(),
                        });
                        let interface = SwfsMountManager::mount_registered(
                            instance.instance_id,
                            mount_path.clone(),
                            &record.external_dir,
                        )
                        .await?;
                        swfs.push((
                            mount_path,
                            SwfsMount {
                                instance_id: instance.instance_id,
                                interface,
                                record,
                            },
                        ));
                    }
                    VfsType::None => {}
                    // Only Linux serves FUSE instances.
                    #[cfg(not(target_os = "linux"))]
                    VfsType::Fuse => {}
                }
            }
        }
        Ok((swfs, pending_fuse, registered, shared))
    }

    /// As [`Self::mounts_of_entries_skipping`], skipping no instance.
    #[cfg(feature = "test-util")]
    pub fn mounts_of_entries(
        entries: &[SharedStoreRegistryEntry],
        state: &mut RootState,
    ) -> impl Future<
        Output = Result<
            (
                FoundSwfsMounts,
                Vec<PendingFuseMount>,
                RegisteredInstances,
                bool,
            ),
            MountManagerError,
        >,
    > {
        Self::mounts_of_entries_skipping(entries, state, |_| false)
    }

    /// The mounts [`Self::mounts_of_entries_skipping`] finds in `entries` with `skip`, in the
    /// external root `state`, with what interrupted clones left among its external directories
    /// removed once they are read under shared access.
    #[lore_macro::test_pub]
    async fn registered_mounts(
        entries: &[SharedStoreRegistryEntry],
        state: &mut RootState,
        skip: impl Fn(&Path) -> bool,
    ) -> Result<(FoundSwfsMounts, Vec<PendingFuseMount>), MountManagerError> {
        let (swfs, pending_fuse, registered, shared) =
            Self::mounts_of_entries_skipping(entries, state, skip).await?;
        #[cfg(target_os = "linux")]
        if shared {
            Box::pin(crate::fs::fuse::mount_manager::remove_interrupted_clones(
                state,
                registered.as_ref(),
            ))
            .await;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (registered, shared);
        Ok((swfs, pending_fuse))
    }

    /// The manager of a starting service, with every registered instance mounted, as
    /// [`Self::registered_mounts`] finds them, and what interrupted clones left removed.
    pub async fn initialize() -> Result<Arc<Self>, MountManagerError> {
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<MountManagerError>("Unable to load shared store registry")?;
        let mut state = resolve().forward::<MountManagerError>("resolving the external root")?;
        let (swfs, pending_fuse) =
            Self::registered_mounts(registry.entries(), &mut state, |_| false).await?;
        let manager = Arc::new(MountManager {
            swfs: SwfsMountManager::with_mounts(swfs).await?,
            #[cfg(target_os = "linux")]
            fuse: FuseMountManager::new(lore_base::runtime::try_lore_context()),
        });
        #[cfg(target_os = "linux")]
        manager.fuse.remount_instances(pending_fuse).await;
        #[cfg(not(target_os = "linux"))]
        let _ = pending_fuse;
        Ok(manager)
    }

    /// Mounts the registered instances of the caller's global data directory into this running
    /// manager, as [`Self::initialize`] mounts them at a start: SWFS mounts join the live SWFS
    /// manager, and FUSE mounts run through its remount. For a service whose start skipped them
    /// while a move of the external root waited for recovery, and what interrupted clones left is
    /// removed, as at a start. An instance whose path this manager mounts already is left as it
    /// is.
    pub async fn mount_registered_instances(&self) -> Result<(), MountManagerError> {
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<MountManagerError>("Unable to load shared store registry")?;
        let mut state = resolve().forward::<MountManagerError>("resolving the external root")?;
        let mounted = |path: &Path| matches!(self.is_mounted(path), Ok(true));
        let (swfs, pending_fuse) =
            Self::registered_mounts(registry.entries(), &mut state, mounted).await?;
        self.swfs.add_mounts(swfs).await?;
        #[cfg(target_os = "linux")]
        self.fuse.remount_instances(pending_fuse).await;
        #[cfg(not(target_os = "linux"))]
        let _ = pending_fuse;
        Ok(())
    }

    /// The SWFS mounts.
    pub fn swfs(&self) -> &SwfsMountManager {
        &self.swfs
    }

    /// The FUSE mounts.
    #[cfg(target_os = "linux")]
    pub fn fuse(&self) -> &FuseMountManager {
        &self.fuse
    }

    /// The external `.lore` directory of the instance mounted at `repository_path`, from the
    /// external directory its mount recorded, which resolves nothing: the root was available at
    /// mount, and stays in place while the service runs. `None` where nothing read-write is
    /// mounted there.
    pub fn check_for_external_lore_dir(
        &self,
        repository_path: &Path,
    ) -> Result<Option<PathBuf>, MountManagerError> {
        Ok(self
            .record_at(repository_path)?
            .map(|record| record.external_dir.join(DOT_LORE)))
    }

    /// What the read-write mount at `repository_path` recorded of where its instance lives, if
    /// one is mounted there.
    pub fn record_at(
        &self,
        repository_path: &Path,
    ) -> Result<Option<Arc<MountRecord>>, MountManagerError> {
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        #[cfg(target_os = "linux")]
        if let Some(mount) = self.fuse.mounted_at(&mount_path) {
            return Ok(mount.record().cloned());
        }
        Ok(self.swfs.record_at(&mount_path))
    }

    pub fn filesystem_provider(
        &self,
        repository_path: &Path,
    ) -> Result<Arc<dyn FilesystemProvider>, MountManagerError> {
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        #[cfg(target_os = "linux")]
        if let Some(mount) = self.fuse.mounted_at(&mount_path) {
            return Ok(Arc::new(FuseFilesystemProvider::new(mount)));
        }
        match self.swfs.interface_at(&mount_path) {
            Some(interface) => Ok(Arc::new(SwfsFilesystem::new(
                mount_path.as_ref(),
                interface,
            ))),
            None => Err(InvalidPath {
                path: mount_path.as_ref().display().to_string(),
            }
            .into()),
        }
    }

    /// Check if a path is currently mounted.
    pub fn is_mounted(&self, path: &Path) -> Result<bool, MountManagerError> {
        Ok(self.mounted(path)?.is_some())
    }

    /// The instance mounted at `path` and the VFS mounting it, if any.
    pub fn mounted(&self, path: &Path) -> Result<Option<(InstanceId, VfsType)>, MountManagerError> {
        let mount_path =
            MountPath::new(path).forward::<MountManagerError>("Failed to make mount path")?;
        #[cfg(target_os = "linux")]
        if let Some(instance_id) = self.fuse.instance_at(&mount_path) {
            return Ok(Some((instance_id, VfsType::Fuse)));
        }
        Ok(self
            .swfs
            .instance_at(&mount_path)
            .map(|instance_id| (instance_id, VfsType::Swfs)))
    }

    /// Every mount, by path, with the instance it serves and the VFS mounting it.
    pub fn mounts(&self) -> Vec<(PathBuf, InstanceId, VfsType)> {
        let swfs = self.swfs.mounts().into_iter();
        let mounts = swfs.map(|(path, instance_id)| (path, instance_id, VfsType::Swfs));
        #[cfg(target_os = "linux")]
        let mounts = mounts.chain(
            self.fuse
                .mounts()
                .into_iter()
                .map(|(path, instance_id)| (path, instance_id, VfsType::Fuse)),
        );
        mounts.collect()
    }
}

#[cfg(feature = "test-util")]
impl MountManager {
    pub fn for_test() -> Arc<Self> {
        Arc::new(MountManager {
            swfs: SwfsMountManager::new(),
            #[cfg(target_os = "linux")]
            fuse: FuseMountManager::new(None),
        })
    }
}

/// The lookup of `instance_id` in `state`, which is resolved again under shared access at the
/// first instance whose lookup is present, as `shared` records. Fails where it is refused.
fn lookup_under_shared(
    state: &mut RootState,
    shared: &mut bool,
    instance_id: InstanceId,
) -> Result<Lookup, crate::global::external_dir::ExternalDirError> {
    let lookup = state.lookup(instance_id);
    if *shared || !matches!(lookup, Lookup::Present(_)) {
        return Ok(lookup);
    }
    *state = resolve_shared_in(state.global.clone())?;
    *shared = true;
    Ok(state.lookup(instance_id))
}

/// The external directory that holds the Lore directory `dot_lore`.
pub(crate) fn external_dir_of(dot_lore: &Path) -> Result<PathBuf, MountManagerError> {
    dot_lore.parent().map(Path::to_path_buf).ok_or_else(|| {
        MountManagerError::internal(format!("{} has no external directory", dot_lore.display()))
    })
}
