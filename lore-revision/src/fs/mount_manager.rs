// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service's mounts. Each VFS has a manager of its own, which alone mounts, moves and
//! unmounts its instances: [`SwfsMountManager`] for SWFS and, on Linux, `FuseMountManager` for
//! FUSE. This one holds both, finds every registered instance once as the service starts and hands
//! each to its manager, and answers what is mounted at a path, whichever VFS mounted it.

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
use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::filesystem::SwfsFilesystem;
use crate::fs::swfs::mount_manager::SwfsMountManager;
use crate::fs::swfs::paths::MountPath;
use crate::global::external_dir::check_for_external_lore_dir;
use crate::instance::InstanceId;
#[cfg(target_os = "linux")]
use crate::instance::InstanceMetadata;
use crate::instance::list_instances;
use crate::lore_warn;
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

/// A FUSE instance found at startup, mounted once the manager exists.
#[lore_macro::test_pub]
#[cfg(target_os = "linux")]
pub(crate) struct PendingFuseMount {
    pub(crate) repository: Arc<RepositoryContext>,
    pub(crate) instance: InstanceMetadata,
    pub(crate) dot_lore: PathBuf,
    pub(crate) config: RepositoryConfig,
}

#[lore_macro::test_pub]
#[cfg(not(target_os = "linux"))]
struct PendingFuseMount;

/// The mounts a start made of its registered SWFS instances, not yet ready, by path.
type FoundSwfsMounts = Vec<(MountPath, InstanceId, Arc<SwfsInterface>)>;

pub struct MountManager {
    swfs: SwfsMountManager,
    #[cfg(target_os = "linux")]
    fuse: FuseMountManager,
}

impl MountManager {
    /// Mounts the SWFS instances of every store in `entries` and finds its FUSE instances, which
    /// are mounted once the manager exists. A store that cannot be opened or listed, and an
    /// instance whose external directory or config cannot be read, are logged and skipped rather
    /// than stopping the rest.
    #[lore_macro::test_pub]
    async fn mounts_of_entries(
        entries: &[SharedStoreRegistryEntry],
    ) -> Result<(FoundSwfsMounts, Vec<PendingFuseMount>), MountManagerError> {
        let mut swfs = Vec::new();
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut pending_fuse = Vec::new();
        for entry in entries {
            let entry_repository = match entry.create_null_repository_context().await {
                Ok(repository) => repository,
                Err(error) => {
                    lore_warn!("Unable to open shared store at {}: {error}", entry.path());
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
                    continue;
                }
            };
            for instance in &instances {
                let dot_lore = match check_for_external_lore_dir(instance.instance_id) {
                    Ok(Some(dot_lore)) => dot_lore,
                    Ok(None) => continue,
                    Err(error) => {
                        lore_warn!(
                            "Unable to find the external .lore directory of instance at {}: {error}",
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
                        if remove_deleted_instance(instance).await {
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
                        });
                    }
                    VfsType::Swfs => {
                        let mount_path =
                            MountPath::new(Path::new(&instance.path))
                                .forward::<MountManagerError>("Failed to make mount path")?;
                        let mount = SwfsMountManager::mount_registered(
                            instance.instance_id,
                            mount_path.clone(),
                        )
                        .await?;
                        swfs.push((mount_path, instance.instance_id, mount));
                    }
                    VfsType::None => {}
                    // Only Linux serves FUSE instances.
                    #[cfg(not(target_os = "linux"))]
                    VfsType::Fuse => {}
                }
            }
        }
        Ok((swfs, pending_fuse))
    }

    pub async fn initialize() -> Result<Arc<Self>, MountManagerError> {
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<MountManagerError>("Unable to load shared store registry")?;
        let (swfs, pending_fuse) = Self::mounts_of_entries(registry.entries()).await?;
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

    pub fn check_for_external_lore_dir(
        &self,
        repository_path: &Path,
    ) -> Result<Option<PathBuf>, MountManagerError> {
        match self.mounted(repository_path)? {
            Some((instance_id, _)) => check_for_external_lore_dir(instance_id)
                .forward::<MountManagerError>(
                    "Unable to find external lore directory for expected instance ID",
                ),
            None => Ok(None),
        }
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
