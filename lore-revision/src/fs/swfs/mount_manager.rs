use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use lore_base::error::InvalidPath;
use lore_error_set::ForwardStrict;
use lore_error_set::error_set;

use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::api_interface::WrappedHandle;
use crate::fs::swfs::api_interface::swfs_api::SWFSHandle;
use crate::fs::swfs::filesystem::SwfsFilesystem;
use crate::fs::swfs::mount_resources::MountResources;
use crate::fs::swfs::paths::MountPath;
use crate::global::external_dir::check_for_external_lore_dir;
use crate::global::external_dir::external_dir_for_instance;
use crate::instance::InstanceId;
use crate::instance::list_instances;
use crate::lore::RepositoryId;
use crate::lore_warn;
use crate::repository::DOT_LORE;
use crate::repository::RepositoryConfig;
use crate::repository::load_repository_config_from_dot_dir;
use crate::shared_store::registry::SharedStoreRegistry;
use crate::shared_store::registry::SharedStoreRegistryEntry;
use crate::util::config::SaveableConfig;

pub struct Mount {
    instance_id: InstanceId,
    mount: Arc<SwfsInterface>,
}

#[error_set]
pub enum MountManagerError {
    InvalidPath,
}

pub struct MountManager {
    mounted_paths: DashMap<MountPath, Mount>,
    swfs_id_lookup: DashMap<WrappedHandle, Arc<SwfsInterface>>,
}

impl MountManager {
    /// Mounts every SWFS instance of the stores in `entries`. A store that cannot be opened or
    /// listed, and an instance whose config cannot be read, are logged and skipped rather than
    /// stopping the rest.
    #[lore_macro::test_pub]
    async fn mounts_of_entries(
        entries: &[SharedStoreRegistryEntry],
    ) -> Result<DashMap<MountPath, Mount>, MountManagerError> {
        let mounted_paths = DashMap::new();
        for entry in entries {
            let Ok(entry_repository) =
                entry
                    .create_null_repository_context()
                    .await
                    .inspect_err(|error| {
                        lore_warn!("Unable to open shared store at {}: {error}", entry.path());
                    })
            else {
                continue;
            };
            let Ok(instances) = list_instances(&entry_repository)
                .await
                .inspect_err(|error| {
                    lore_warn!(
                        "Unable to list instances of shared store at {}: {error}",
                        entry.path()
                    );
                })
            else {
                continue;
            };
            for instance in &instances {
                let Some(dot_lore) =
                    check_for_external_lore_dir(instance.instance_id)
                        .forward::<MountManagerError>("Unable to find external .lore directory")?
                else {
                    continue;
                };
                let Ok(config) =
                    load_repository_config_from_dot_dir(&dot_lore).inspect_err(|error| {
                        lore_warn!(
                            "Unable to load the repository config of instance {} at {}: {error}",
                            instance.instance_id,
                            instance.path
                        );
                    })
                else {
                    continue;
                };
                if config.is_swfs() {
                    let mount_path = MountPath::new(Path::new(&instance.path))
                        .forward::<MountManagerError>("Failed to make mount path")?;
                    let mount = SwfsInterface::mount(
                        MountResources::premount_instance(instance.instance_id, mount_path.clone())
                            .await
                            .forward::<MountManagerError>("Failed gathering premount resources")?,
                    )
                    .forward::<MountManagerError>("Failed to mount")?;

                    mounted_paths.insert(
                        mount_path,
                        Mount {
                            instance_id: instance.instance_id,
                            mount,
                        },
                    );
                }
            }
        }
        Ok(mounted_paths)
    }

    pub async fn initialize() -> Result<Arc<Self>, MountManagerError> {
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<MountManagerError>("Unable to load shared store registry")?;
        let mounted_paths = Self::mounts_of_entries(registry.entries()).await?;
        let lookup = DashMap::new();
        for mount in mounted_paths.iter() {
            mount
                .mount
                .wait_for_ready()
                .await
                .forward::<MountManagerError>("Failed waiting for mount to be ready")?;
            lookup.insert(mount.mount.lookup_key(), mount.mount.clone());
        }
        Ok(Arc::new(MountManager {
            mounted_paths,
            swfs_id_lookup: lookup,
        }))
    }

    pub fn check_for_external_lore_dir(
        &self,
        repository_path: &Path,
    ) -> Result<Option<PathBuf>, MountManagerError> {
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        if let Some(mount) = self.mounted_paths.get(&mount_path) {
            check_for_external_lore_dir(mount.instance_id).forward::<MountManagerError>(
                "Unable to find external lore directory for expected instance ID",
            )
        } else {
            Ok(None)
        }
    }

    pub fn get_mount_filesystem_provider(
        &self,
        repository_path: &Path,
    ) -> Result<Arc<SwfsFilesystem>, MountManagerError> {
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        let mount = self
            .mounted_paths
            .get(&mount_path)
            .ok_or_else(|| InvalidPath {
                path: mount_path.as_ref().display().to_string(),
            })?;
        Ok(Arc::new(SwfsFilesystem::new(
            mount_path.as_ref(),
            mount.mount.clone(),
        )))
    }

    pub async fn create_mount(
        &self,
        repository_path: &Path,
        repository_config: &RepositoryConfig,
        repository_id: RepositoryId,
        instance_id: InstanceId,
    ) -> Result<PathBuf, MountManagerError> {
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        if self.mounted_paths.contains_key(&mount_path) {
            return Err(MountManagerError::internal(format!(
                "Mount already exists for path {}",
                repository_path.display()
            )));
        }
        let dot_lore = external_dir_for_instance(instance_id)
            .forward::<MountManagerError>("Unable to get external directory")?
            .join(DOT_LORE);
        let mount = SwfsInterface::mount(
            MountResources::premount_instance_with_config(
                repository_id,
                instance_id,
                mount_path.clone(),
                repository_config,
            )
            .await
            .forward::<MountManagerError>("Failed gathering premount resources")?,
        )
        .forward::<MountManagerError>("Failed to mount")?;
        self.attach(mount_path, instance_id, mount).await?;
        Ok(dot_lore)
    }

    pub fn get_interface_from_swfs_handle(&self, handle: SWFSHandle) -> Option<Arc<SwfsInterface>> {
        self.swfs_id_lookup
            .get(&WrappedHandle::new(handle))
            .map(|entry| entry.value().clone())
    }

    /// Returns the number of currently mounted SWFS paths.
    pub fn mount_count(&self) -> usize {
        self.mounted_paths.len()
    }

    /// Check if a path is currently mounted.
    pub fn is_mounted(&self, path: &Path) -> Result<bool, MountManagerError> {
        let mount_path =
            MountPath::new(path).forward::<MountManagerError>("Failed to make mount path")?;
        Ok(self.mounted_paths.contains_key(&mount_path))
    }

    /// The instance mounted at `path`, if any.
    pub fn mounted_instance(&self, path: &Path) -> Result<Option<InstanceId>, MountManagerError> {
        let mount_path =
            MountPath::new(path).forward::<MountManagerError>("Failed to make mount path")?;
        Ok(self
            .mounted_paths
            .get(&mount_path)
            .map(|mount| mount.instance_id))
    }

    /// Remove a mount, triggering cleanup/swfsClose when Arc drops.
    /// Returns the `instance_id` if a mount existed at the path.
    pub fn remove_mount(&self, path: &Path) -> Result<Option<InstanceId>, MountManagerError> {
        let mount_path =
            MountPath::new(path).forward::<MountManagerError>("Failed to make mount path")?;
        if let Some((_, mount)) = self.mounted_paths.remove(&mount_path) {
            self.swfs_id_lookup.remove(&mount.mount.lookup_key());
            // Closed explicitly: other holders of the interface would keep the path mounted.
            mount.mount.close();
            Ok(Some(mount.instance_id))
        } else {
            Ok(None)
        }
    }

    /// Mount `instance_id` at `mount_path`, record it, and wait for SWFS to be ready.
    async fn mount_instance(
        &self,
        instance_id: InstanceId,
        mount_path: MountPath,
    ) -> Result<(), MountManagerError> {
        let mount = SwfsInterface::mount(
            MountResources::premount_instance(instance_id, mount_path.clone())
                .await
                .forward::<MountManagerError>("Failed gathering premount resources")?,
        )
        .forward::<MountManagerError>("Failed to mount")?;
        self.attach(mount_path, instance_id, mount).await
    }

    /// Record a fresh `mount` of `instance_id` at `mount_path` and wait for SWFS to be ready. If
    /// SWFS does not become ready the mount is closed and no longer recorded.
    async fn attach(
        &self,
        mount_path: MountPath,
        instance_id: InstanceId,
        mount: Arc<SwfsInterface>,
    ) -> Result<(), MountManagerError> {
        let handle = mount.lookup_key();
        // Recorded before SWFS is ready, as it can call back as soon as it is mounted.
        self.swfs_id_lookup.insert(handle.clone(), mount.clone());
        let displaced = self.mounted_paths.insert(
            mount_path.clone(),
            Mount {
                instance_id,
                mount: mount.clone(),
            },
        );
        if displaced.is_some() {
            return Err(MountManagerError::internal(format!(
                "Mount double created for path {}",
                mount_path.as_ref().display()
            )));
        }
        if let Err(error) = mount.wait_for_ready().await {
            self.mounted_paths.remove(&mount_path);
            self.swfs_id_lookup.remove(&handle);
            mount.close();
            return Err(MountManagerError::internal(format!(
                "Failed waiting for mount to be ready: {error}"
            )));
        }
        Ok(())
    }

    /// Move a mount from one path to another by unmounting it from `from_path` and mounting the
    /// same instance at `to_path`. SWFS is given the mount path when a mount is created, so it
    /// cannot be pointed at a new one in place.
    ///
    /// If the target cannot be mounted, the instance is mounted at `from_path` again.
    pub async fn remount(&self, from_path: &Path, to_path: &Path) -> Result<(), MountManagerError> {
        let from_mount_path = MountPath::new(from_path)
            .forward::<MountManagerError>("Failed to make from mount path")?;
        let to_mount_path =
            MountPath::new(to_path).forward::<MountManagerError>("Failed to make to mount path")?;

        // The mount is closed, not just dropped: callers still hold references to the interface,
        // and the source path stays mounted until it is closed.
        let Some(instance_id) = self.remove_mount(from_path)? else {
            return Err(MountManagerError::internal(format!(
                "Mount not found at path {}",
                from_path.display()
            )));
        };

        let Err(error) = self.mount_instance(instance_id, to_mount_path).await else {
            return Ok(());
        };
        self.mount_instance(instance_id, from_mount_path)
            .await
            .map_err(|restore_error| {
                MountManagerError::internal(format!(
                    "Failed to mount at {}: {error}; restoring the mount at {} also failed: {restore_error}",
                    to_path.display(),
                    from_path.display()
                ))
            })?;
        Err(error)
    }

    /// Unmounts the mounts the service has to release before it stops, as the service does when
    /// it stops, detaching a busy one lazily with `force`. SWFS mounts are left as they are, and
    /// they are the only mounts, so this unmounts nothing.
    #[allow(clippy::unused_async)]
    pub async fn unmount_all(&self, _force: bool) -> Result<(), MountManagerError> {
        Ok(())
    }
}
