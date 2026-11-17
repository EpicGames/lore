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
use crate::global::external_dir::canonical_global;
use crate::global::external_dir::resolve::Lookup;
use crate::global::external_dir::resolve::RootState;
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
    #[lore_macro::test_pub]
    async fn mounts_of_entries(
        entries: &[SharedStoreRegistryEntry],
        state: &RootState,
    ) -> Result<(FoundSwfsMounts, Vec<PendingFuseMount>, RegisteredInstances), MountManagerError>
    {
        let mut swfs = Vec::new();
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut pending_fuse = Vec::new();
        let mut registered = Some(BTreeSet::new());
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
                let dot_lore = match state.lookup(instance.instance_id) {
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
        Ok((swfs, pending_fuse, registered))
    }

    pub async fn initialize() -> Result<Arc<Self>, MountManagerError> {
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<MountManagerError>("Unable to load shared store registry")?;
        let state = resolve().forward::<MountManagerError>("resolving the external root")?;
        let (swfs, pending_fuse, registered) =
            Self::mounts_of_entries(registry.entries(), &state).await?;
        #[cfg(target_os = "linux")]
        Box::pin(crate::fs::fuse::mount_manager::remove_interrupted_clones(
            &state,
            registered.as_ref(),
        ))
        .await;
        #[cfg(not(target_os = "linux"))]
        let _ = registered;
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

/// The external directory that holds the Lore directory `dot_lore`.
pub(crate) fn external_dir_of(dot_lore: &Path) -> Result<PathBuf, MountManagerError> {
    dot_lore.parent().map(Path::to_path_buf).ok_or_else(|| {
        MountManagerError::internal(format!("{} has no external directory", dot_lore.display()))
    })
}
