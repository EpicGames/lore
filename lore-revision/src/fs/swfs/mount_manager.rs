// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service's SWFS mounts, and the handles SWFS calls back through.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use lore_base::error::NotSupported;
use lore_error_set::ForwardStrict;

use crate::fs::mount_manager::MountManagerError;
use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::api_interface::WrappedHandle;
use crate::fs::swfs::api_interface::swfs_api::SWFSHandle;
use crate::fs::swfs::mount_resources::MountResources;
use crate::fs::swfs::paths::MountPath;
use crate::global::external_dir::external_dir_for_instance;
use crate::instance::InstanceId;
use crate::lore::RepositoryId;
use crate::repository::DOT_LORE;
use crate::repository::RepositoryConfig;
use crate::repository::VfsType;

/// An SWFS mount: the instance it serves and the interface SWFS calls back through.
struct SwfsMount {
    instance_id: InstanceId,
    interface: Arc<SwfsInterface>,
}

/// The service's SWFS mounts.
///
/// A path holds one mount, which this manager checks against its own mounts alone: no platform
/// mounts both SWFS and FUSE yet, as SWFS links on Windows only and FUSE runs on Linux only.
pub struct SwfsMountManager {
    mounts: DashMap<MountPath, SwfsMount>,
    handles: DashMap<WrappedHandle, Arc<SwfsInterface>>,
    /// Held across the check against [`Self::MOUNT_LIMIT`] and the mount that follows it, so two
    /// instances created at once cannot both take the last place.
    creating: tokio::sync::Mutex<()>,
}

impl SwfsMountManager {
    /// The most SWFS instances a service mounts. The SWFS driver attaches about eight mounts, and
    /// the library parks the process that asks for one more, so the client hangs or loses its
    /// connection with no way back. The driver's limit is machine-wide, which this count of the
    /// service's own mounts does not see.
    pub(crate) const MOUNT_LIMIT: usize = 8;

    pub(crate) fn new() -> Self {
        Self {
            mounts: DashMap::new(),
            handles: DashMap::new(),
            creating: tokio::sync::Mutex::new(()),
        }
    }

    /// A manager holding the mounts a start made of its registered instances, `found`, each
    /// recorded once SWFS is ready. A later registration at a path displaces an earlier one, whose
    /// mount is dropped unused.
    pub(crate) async fn with_mounts(
        found: Vec<(MountPath, InstanceId, Arc<SwfsInterface>)>,
    ) -> Result<Self, MountManagerError> {
        let mut unique: Vec<(MountPath, InstanceId, Arc<SwfsInterface>)> =
            Vec::with_capacity(found.len());
        for mount in found {
            unique.retain(|(mount_path, _, _)| *mount_path != mount.0);
            unique.push(mount);
        }
        let manager = Self::new();
        for (mount_path, instance_id, interface) in unique {
            interface
                .wait_for_ready()
                .await
                .forward::<MountManagerError>("Failed waiting for mount to be ready")?;
            manager
                .handles
                .insert(interface.lookup_key(), interface.clone());
            manager.mounts.insert(
                mount_path,
                SwfsMount {
                    instance_id,
                    interface,
                },
            );
        }
        Ok(manager)
    }

    /// Mounts the registered instance `instance_id` at `mount_path` through SWFS, without waiting
    /// for SWFS to be ready or recording the mount.
    pub(crate) async fn mount_registered(
        instance_id: InstanceId,
        mount_path: MountPath,
    ) -> Result<Arc<SwfsInterface>, MountManagerError> {
        SwfsInterface::mount(
            MountResources::premount_instance(instance_id, mount_path)
                .await
                .forward::<MountManagerError>("Failed gathering premount resources")?,
        )
        .forward::<MountManagerError>("Failed to mount")
    }

    /// Refuses another SWFS instance once this service mounts [`Self::MOUNT_LIMIT`] of them.
    pub fn refuse_past_mount_limit(&self) -> Result<(), NotSupported> {
        let mounted = self.mount_count();
        if mounted < Self::MOUNT_LIMIT {
            return Ok(());
        }
        Err(NotSupported {
            operation: format!(
                "mounting another SWFS instance: this service already mounts {mounted}, the most \
                 SWFS supports at once"
            ),
        })
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
        let _creating = self.creating.lock().await;
        if self.mounts.contains_key(&mount_path) {
            return Err(MountManagerError::internal(format!(
                "Mount already exists for path {}",
                repository_path.display()
            )));
        }
        let vfs = repository_config.vfs_type();
        if vfs != VfsType::Swfs {
            return Err(MountManagerError::internal(format!(
                "SWFS cannot mount a {vfs} instance"
            )));
        }
        // Create and clone refuse an instance past the limit before making any of it; this
        // catches one that raced another to the last place.
        self.refuse_past_mount_limit()
            .map_err(|refused| MountManagerError::internal(refused.to_string()))?;
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
        self.handles
            .get(&WrappedHandle::new(handle))
            .map(|entry| entry.value().clone())
    }

    /// Returns the number of currently mounted SWFS paths.
    pub fn mount_count(&self) -> usize {
        self.mounts.len()
    }

    /// The mount `path` lies inside, below its root, if any.
    ///
    /// Compared as text, case folded and with separators made alike, rather than canonicalized:
    /// canonicalizing a path inside a mount is what fails.
    pub fn mount_containing(&self, path: &Path) -> Option<PathBuf> {
        let normalize = |path: &Path| {
            let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
            text.strip_prefix(r"\\?\")
                .map_or(text.clone(), str::to_owned)
                .trim_end_matches('\\')
                .to_owned()
        };
        let target = normalize(path);
        self.mounts.iter().find_map(|entry| {
            let root = normalize(entry.key().as_ref());
            target
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with('\\'))
                .then(|| entry.key().as_ref().to_path_buf())
        })
    }

    /// The instance mounted at `mount_path`, if any.
    pub(crate) fn instance_at(&self, mount_path: &MountPath) -> Option<InstanceId> {
        self.mounts.get(mount_path).map(|mount| mount.instance_id)
    }

    /// The interface of the mount at `mount_path`, if any.
    pub(crate) fn interface_at(&self, mount_path: &MountPath) -> Option<Arc<SwfsInterface>> {
        self.mounts
            .get(mount_path)
            .map(|mount| mount.interface.clone())
    }

    /// Every mount, by path, with the instance it serves.
    pub fn mounts(&self) -> Vec<(PathBuf, InstanceId)> {
        self.mounts
            .iter()
            .map(|entry| (entry.key().as_ref().to_path_buf(), entry.instance_id))
            .collect()
    }

    /// Remove a mount, closing it. Returns the `instance_id` if a mount existed at the path.
    pub fn remove_mount(&self, path: &Path) -> Result<Option<InstanceId>, MountManagerError> {
        let mount_path =
            MountPath::new(path).forward::<MountManagerError>("Failed to make mount path")?;
        if let Some((_, mount)) = self.mounts.remove(&mount_path) {
            self.handles.remove(&mount.interface.lookup_key());
            // Closed explicitly: other holders of the interface would keep the path mounted.
            mount.interface.close();
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
        let mount = Self::mount_registered(instance_id, mount_path.clone()).await?;
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
        self.handles.insert(handle.clone(), mount.clone());
        let displaced = self.mounts.insert(
            mount_path.clone(),
            SwfsMount {
                instance_id,
                interface: mount.clone(),
            },
        );
        if displaced.is_some() {
            return Err(MountManagerError::internal(format!(
                "Mount double created for path {}",
                mount_path.as_ref().display()
            )));
        }
        if let Err(error) = mount.wait_for_ready().await {
            self.mounts.remove(&mount_path);
            self.handles.remove(&handle);
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

    /// Closes every SWFS mount, for the service to call as it exits, however it was stopped.
    ///
    /// The mount manager is a static, which is never dropped, so a mount not closed here outlives
    /// the process only in the driver, which does not tear down a mount whose client went away
    /// without closing it. On a blocking thread, because closing a mount waits for its uses in
    /// progress (see [`SwfsInterface::close`]) and then on SWFS, which allows a client still
    /// writing through the mount 10 seconds.
    pub async fn close_all(&self) -> Result<(), MountManagerError> {
        let mounts: Vec<Arc<SwfsInterface>> = self
            .mounts
            .iter()
            .map(|mount| Arc::clone(&mount.interface))
            .collect();
        lore_base::lore_spawn_blocking!(move || {
            for mount in mounts {
                mount.close();
            }
        })
        .await
        .map_err(|error| MountManagerError::internal(format!("Closing SWFS mounts: {error}")))
    }
}
