// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service's FUSE mounts: the virtual clones it serves, which it mounts, moves and unmounts,
//! and the stops that release them.

use std::any::Any;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use lore_base::error::InvalidPath;
use lore_base::error::MountsBusy;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::ForwardAny;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;

use crate::fs::fuse::mount::FuseMount;
use crate::fs::mount_manager::MountManagerError;
use crate::fs::mount_manager::PendingFuseMount;
use crate::fs::swfs::paths::MountPath;
use crate::global::external_dir::external_dir_for_instance;
use crate::instance::InstanceId;
use crate::instance::InstanceMetadata;
use crate::instance::InstanceStaleness;
use crate::instance::instance_staleness;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_warn;
use crate::repository::DOT_LORE;
use crate::repository::RepositoryConfig;
use crate::repository::RepositoryContext;
use crate::repository::load_repository_config_from_dot_dir;
use crate::state::State;

/// Where a stop runs a test's hook, as a command racing the stop would, or a move that a stop
/// can race; no lock is held at any.
#[doc(hidden)]
#[cfg(feature = "test-util")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopStep {
    /// The stop has set its flags and taken its snapshot of the mounts; it unmounts them next.
    Snapshot,
    /// The stop has tried every mount in its snapshot; it forgets or remounts them next.
    Unmounted,
    /// A refused stop is about to mount again a mount it unmounted and still tracks.
    Remounting,
    /// A move, not a stop, has unmounted its instance and mounts it at its target next.
    MoveUnmounted,
}

/// A test's hook into a stop ([`StopStep`]).
#[doc(hidden)]
#[cfg(feature = "test-util")]
pub type StopHook = Arc<dyn Fn(StopStep) -> crate::BoxFuture<'static, ()> + Send + Sync>;

/// Makes the directory a moved FUSE instance mounts on at `path`, or takes the empty one a move
/// away from `path` left. Reports whether it made one.
async fn make_mount_point(path: &Path) -> std::io::Result<bool> {
    let io = lore_io::IoDriver::global();
    if io.metadata(path).await.is_err() {
        return io.create_dir_all(path).await.map(|()| true);
    }
    match io.read_dir(path).await?.next().await {
        None => Ok(false),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::DirectoryNotEmpty,
            "the mount point is not empty",
        )),
    }
}

/// Removes the mount point [`make_mount_point`] made at `path` for a move that did not go ahead.
/// One left behind is only an empty directory, so a failure is reported and nothing more.
async fn remove_made_mount_point(path: &Path, made: bool) {
    if !made {
        return;
    }
    if let Err(error) = lore_io::IoDriver::global().remove_dir(path).await {
        lore_warn!(
            "Unable to remove {}, made for a move that did not go ahead: {error}",
            path.display()
        );
    }
}

/// Removes the external directory of a FUSE instance whose mount point was deleted, so the
/// instance is not mounted again; the next prune of its store retires the registration. Reports
/// whether the mount point was deleted. A path that cannot be reached is not deleted.
pub(crate) async fn remove_deleted_instance(instance: &InstanceMetadata) -> bool {
    if instance_staleness(&instance.path, instance.instance_id).await
        != InstanceStaleness::PathMissing
    {
        return false;
    }
    match external_dir_for_instance(instance.instance_id) {
        Ok(external) => match lore_io::IoDriver::global().remove_dir_all(&external).await {
            Ok(()) => lore_info!(
                "Removed FUSE instance {} whose mount point {} was deleted",
                instance.instance_id,
                instance.path
            ),
            Err(error) => lore_warn!(
                "Unable to remove {} of FUSE instance {} whose mount point was deleted: {error}",
                external.display(),
                instance.instance_id
            ),
        },
        Err(error) => lore_warn!(
            "Unable to locate the external directory of FUSE instance {}: {error}",
            instance.instance_id
        ),
    }
    true
}

/// The service's FUSE mounts.
///
/// A path holds one mount, which this manager checks against its own mounts alone: no platform
/// mounts both FUSE and SWFS yet, as SWFS links on Windows only.
#[lore_macro::test_pub]
pub struct FuseMountManager {
    mounts: DashMap<MountPath, Arc<FuseMount>>,
    /// The service's execution context. FUSE providers are built under it, because each keeps
    /// the context current at construction for the whole life of its mount.
    context: Option<Arc<dyn Any + Send + Sync>>,
    /// Set from the moment a stop takes its snapshot of the mounts until the stop is refused.
    stopping: AtomicBool,
    /// Set once a stop goes ahead and never cleared, so nothing mounts while the service exits,
    /// whatever another stop running alongside it decides.
    exiting: AtomicBool,
    #[cfg(feature = "test-util")]
    stop_hook: std::sync::OnceLock<StopHook>,
}

impl FuseMountManager {
    pub(crate) fn new(context: Option<Arc<dyn Any + Send + Sync>>) -> Self {
        Self {
            mounts: DashMap::new(),
            context,
            stopping: AtomicBool::new(false),
            exiting: AtomicBool::new(false),
            #[cfg(feature = "test-util")]
            stop_hook: std::sync::OnceLock::new(),
        }
    }

    /// Mounts the FUSE instances a start found. One instance that fails to mount must not keep
    /// the service from serving the others.
    pub(crate) async fn remount_instances(&self, pending: Vec<PendingFuseMount>) {
        for fuse in pending {
            if let Err(error) = self
                .remount_instance(
                    fuse.repository,
                    &fuse.instance,
                    &fuse.dot_lore,
                    &fuse.config,
                )
                .await
            {
                lore_warn!(
                    "Unable to mount FUSE instance at {}: {error}",
                    fuse.instance.path
                );
            }
        }
    }

    /// Mounts a fresh FUSE instance at `repository_path`, empty until its first swap.
    pub async fn create_mount(
        &self,
        repository: Arc<RepositoryContext>,
        repository_path: &Path,
        repository_config: &RepositoryConfig,
        instance_id: InstanceId,
    ) -> Result<(), MountManagerError> {
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        if self.mounts.contains_key(&mount_path) {
            return Err(MountManagerError::internal(format!(
                "Mount already exists for path {}",
                repository_path.display()
            )));
        }
        let base_time = repository_config.vfs_base_time();
        let external_dir = external_dir_for_instance(instance_id)
            .forward::<MountManagerError>("Unable to get external directory")?;
        // Made in the caller's context, as the external directory is: its connect authenticates
        // as the caller, with the credentials the call supplied, and the mount serving that
        // caller's instance holds the connection for its life.
        let remote = repository.serving_remote();
        // Boxed: the mount is a large future, which the context it runs under would hold again.
        let mount = self
            .in_serving_context(Box::pin(FuseMount::mount_writable(
                (repository, remote),
                State::new(),
                mount_path.as_ref(),
                instance_id,
                base_time,
                external_dir,
            )))
            .await
            .forward_any::<MountManagerError>("Failed to mount FUSE instance")?;
        self.mounts.insert(mount_path.clone(), mount);
        self.refuse_while_stopping(&mount_path).await
    }

    /// Unmounts and forgets the mount just made at `mount_path` if a stop has taken its snapshot
    /// of the mounts since, which may have missed it, or one has gone ahead, and fails. Checked
    /// after the mount is recorded, so a stop that sets the flag later finds the mount in its
    /// snapshot.
    #[lore_macro::test_pub]
    async fn refuse_while_stopping(&self, mount_path: &MountPath) -> Result<(), MountManagerError> {
        if !self.stopping.load(Ordering::SeqCst) && !self.exiting.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Forgets only the mount it tried to detach, not one a refused stop mounted there since.
        let tracked = self.mounted_at(mount_path);
        if let Some(mount) = tracked
            && let Err(error) = self.unmount_tracked(mount_path, &mount, true).await
        {
            lore_warn!(
                "Unable to detach {}, mounted while the service stops: {error}",
                mount_path.as_ref().display()
            );
            self.mounts
                .remove_if(mount_path, |_, entry| Arc::ptr_eq(entry, &mount));
        }
        Err(MountManagerError::internal(format!(
            "the service is stopping, so {} is not mounted",
            mount_path.as_ref().display()
        )))
    }

    /// Mounts an existing FUSE instance at its current revision, first detaching a stale
    /// endpoint a crashed service left at its path. Inode numbers derive from paths, so a
    /// remounted instance answers with the numbers it had before.
    pub async fn remount_instance(
        &self,
        repository: Arc<RepositoryContext>,
        instance: &InstanceMetadata,
        dot_lore: &Path,
        config: &RepositoryConfig,
    ) -> Result<(), MountManagerError> {
        let mount_path = MountPath::new(Path::new(&instance.path))
            .forward::<MountManagerError>("Failed to make mount path")?;
        if self.mounts.contains_key(&mount_path) {
            return Err(MountManagerError::internal(format!(
                "Mount already exists for path {}",
                instance.path
            )));
        }
        let probe = mount_path.as_ref().to_path_buf();
        lore_base::lore_spawn_blocking!(move || {
            if lore_fuse::mount::is_stale_mount(&probe) {
                lore_fuse::mount::unmount_path(&probe, true)
            } else {
                Ok(())
            }
        })
        .await
        .internal("stale mount probe panicked")?
        .internal("Failed to detach a stale mount")?;

        let instance_repository = Arc::new(
            repository
                .mounted_instance_context(
                    mount_path.as_ref(),
                    dot_lore,
                    instance.instance_id,
                    config,
                )
                .forward_any::<MountManagerError>("Failed to open the mounted instance")?,
        );
        let base_time = config.vfs_base_time();
        // Boxed: loading the revision and starting a session make a large future.
        let mount = Box::pin(self.in_service_context(async {
            let (state, _branch) = State::deserialize_current(instance_repository.clone())
                .await
                .forward_any::<MountManagerError>("Failed to load the mounted revision")?;
            let external_dir = external_dir_for_instance(instance.instance_id)
                .forward::<MountManagerError>("Unable to get external directory")?;
            let remote = instance_repository.serving_remote();
            FuseMount::mount_writable(
                (instance_repository, remote),
                state,
                mount_path.as_ref(),
                instance.instance_id,
                base_time,
                external_dir,
            )
            .await
            .forward_any::<MountManagerError>("Failed to mount FUSE instance")
        }))
        .await?;
        self.mounts.insert(mount_path.clone(), mount);
        self.refuse_while_stopping(&mount_path).await
    }

    /// Moves FUSE instance `instance_id`, mounted at `from`, to `to`: a plain unmount, which a
    /// busy mount refuses with nothing changed, then the instance mounted at the revision it served
    /// on a mount point made at `to`.
    ///
    /// The mount point at `from` stays, empty, for the caller to remove once the instance is
    /// registered at `to`. A service that starts before then finds the registered path still
    /// there and mounts the instance at it, where it would take a missing one for a deleted
    /// instance and remove its external directory. For the same reason a move back accepts the
    /// empty mount point the move left.
    ///
    /// A move is refused while the service stops. Where the mount at `to` fails, or a stop
    /// that began after that check refuses it, the mount point made there is removed and the
    /// instance mounted at `from` again. A stop still under way refuses that too, which leaves the
    /// instance for the next start to mount at `from`.
    pub async fn move_mount(
        &self,
        instance_id: InstanceId,
        from: &Path,
        to: &Path,
    ) -> Result<(), MountManagerError> {
        let mount = self
            .mounted(from)
            .filter(|mount| mount.instance_id == instance_id)
            .ok_or_else(|| InvalidPath {
                path: from.display().to_string(),
            })?;
        let from_mount_path =
            MountPath::new(from).forward::<MountManagerError>("making the source mount path")?;
        let to_mount_path =
            MountPath::new(to).forward::<MountManagerError>("making the target mount path")?;
        if self.mounts.contains_key(&to_mount_path) {
            return Err(MountManagerError::internal(format!(
                "Mount already exists for path {}",
                to.display()
            )));
        }
        let dot_lore = external_dir_for_instance(instance_id)
            .forward::<MountManagerError>("locating the instance's external directory")?
            .join(DOT_LORE);
        let config = load_repository_config_from_dot_dir(&dot_lore)
            .forward_any::<MountManagerError>("loading the instance's config")?;
        let to = to_mount_path.as_ref();
        // Built before anything changes, in the service's context as a start builds it: the
        // command's globals, such as `--offline`, must not decide how the mount reaches its remote.
        let repository = self
            .in_service_context(async {
                mount
                    .filesystem
                    .served()
                    .repository
                    .mounted_instance_context(to, &dot_lore, instance_id, &config)
            })
            .await
            .forward_any::<MountManagerError>("opening the instance at its new path")?;
        if self.stopping.load(Ordering::SeqCst) || self.exiting.load(Ordering::SeqCst) {
            return Err(MountManagerError::internal(format!(
                "the service is stopping, so {} is not moved",
                from.display()
            )));
        }
        self.unmount_tracked(&from_mount_path, &mount, false)
            .await?;
        #[cfg(feature = "test-util")]
        self.stop_step(StopStep::MoveUnmounted).await;
        let (made, error) = match make_mount_point(to).await {
            Err(error) => (
                false,
                MountManagerError::internal_with_context(error, "making the mount point"),
            ),
            Ok(made) => {
                let mounted =
                    Box::pin(self.in_service_context(mount.remount_at(to, Arc::new(repository))))
                        .await
                        .forward_any::<MountManagerError>("mounting the instance at its new path");
                let error = match mounted {
                    Ok(remounted) => {
                        self.mounts.insert(to_mount_path.clone(), remounted);
                        match self.refuse_while_stopping(&to_mount_path).await {
                            Ok(()) => return Ok(()),
                            Err(refused) => refused,
                        }
                    }
                    Err(error) => error,
                };
                (made, error)
            }
        };
        remove_made_mount_point(to, made).await;
        let restored = match Box::pin(self.in_service_context(mount.remount())).await {
            Ok(remounted) => {
                self.mounts.insert(from_mount_path.clone(), remounted);
                self.refuse_while_stopping(&from_mount_path).await
            }
            Err(restore) => Err(restore).forward_any::<MountManagerError>("mounting it again"),
        };
        match restored {
            Ok(()) => Err(error),
            // In the message itself: a caller's context would push a context out of it, and this
            // says where the instance is left.
            Err(restore) => Err(MountManagerError::internal(format!(
                "{error}; mounting it at {} again failed too ({restore}), so the service mounts \
                 it there when it starts again",
                from.display()
            ))),
        }
    }

    /// The mount at `repository_path`, if any.
    pub fn mounted(&self, repository_path: &Path) -> Option<Arc<FuseMount>> {
        let mount_path = MountPath::new(repository_path).ok()?;
        self.mounted_at(&mount_path)
    }

    /// The mount at `mount_path`, if any.
    pub(crate) fn mounted_at(&self, mount_path: &MountPath) -> Option<Arc<FuseMount>> {
        self.mounts
            .get(mount_path)
            .map(|entry| entry.value().clone())
    }

    /// The instance mounted at `mount_path`, if any, read without taking a reference to its mount.
    pub(crate) fn instance_at(&self, mount_path: &MountPath) -> Option<InstanceId> {
        self.mounts.get(mount_path).map(|entry| entry.instance_id)
    }

    /// Every mount, by path, with the instance it serves.
    pub fn mounts(&self) -> Vec<(PathBuf, InstanceId)> {
        self.mounts
            .iter()
            .map(|entry| (entry.key().as_ref().to_path_buf(), entry.instance_id))
            .collect()
    }

    /// Unmounts the FUSE instance at `repository_path` and forgets it. A plain unmount of a busy
    /// mount fails and leaves it mounted.
    pub async fn unmount(
        &self,
        repository_path: &Path,
        lazy: bool,
    ) -> Result<(), MountManagerError> {
        let mount = self.mounted(repository_path).ok_or_else(|| InvalidPath {
            path: repository_path.display().to_string(),
        })?;
        let mount_path = MountPath::new(repository_path)
            .forward::<MountManagerError>("Failed to make mount path")?;
        self.unmount_tracked(&mount_path, &mount, lazy).await
    }

    /// Unmounts `mount`, the FUSE mount at `mount_path`, and forgets it. Fails where a mount made
    /// at the path has taken its place, as a refused stop that remounts it does: before the
    /// unmount, which detaches by path and would detach the mount that took the place, or after
    /// it, since the path is then still mounted.
    async fn unmount_tracked(
        &self,
        mount_path: &MountPath,
        mount: &Arc<FuseMount>,
        lazy: bool,
    ) -> Result<(), MountManagerError> {
        if !self.tracks_at(mount_path, mount) {
            let replaced = self.mounts.contains_key(mount_path);
            return Err(MountManagerError::internal(format!(
                "{} {}",
                mount_path.as_ref().display(),
                if replaced {
                    "was mounted again"
                } else {
                    "is no longer mounted"
                }
            )));
        }
        mount
            .unmount(lazy)
            .await
            .forward_any::<MountManagerError>("Failed to unmount FUSE instance")?;
        let forgotten = self
            .mounts
            .remove_if(mount_path, |_, entry| Arc::ptr_eq(entry, mount))
            .is_some();
        if !forgotten && self.mounts.contains_key(mount_path) {
            return Err(MountManagerError::internal(format!(
                "{} was mounted again while it was unmounted",
                mount_path.as_ref().display()
            )));
        }
        Ok(())
    }

    /// Whether the entry at `mount_path` is `mount`, rather than none or a mount made there since.
    fn tracks_at(&self, mount_path: &MountPath, mount: &Arc<FuseMount>) -> bool {
        self.mounts
            .get(mount_path)
            .is_some_and(|entry| Arc::ptr_eq(entry.value(), mount))
    }

    /// Undoes a FUSE clone that failed: unmounts lazily whatever the manager still tracks there,
    /// removes the instance's external directory, and removes an empty mount point this created.
    pub async fn abandon_clone(
        &self,
        repository_path: &Path,
        instance_id: InstanceId,
        created_mount_point: bool,
    ) -> Result<(), MountManagerError> {
        if let Some(mount) = self.mounted(repository_path)
            && let Ok(mount_path) = MountPath::new(repository_path)
            && let Err(error) = self.unmount_tracked(&mount_path, &mount, true).await
        {
            lore_warn!(
                "Lazy unmount failed while abandoning FUSE clone at {}: {error}",
                repository_path.display()
            );
            // Forgets only the clone's own mount, not one made at the path since.
            self.mounts
                .remove_if(&mount_path, |_, entry| Arc::ptr_eq(entry, &mount));
        }
        let external = external_dir_for_instance(instance_id)
            .forward::<MountManagerError>("Locating the abandoned clone's external directory")?;
        if let Err(error) = lore_io::IoDriver::global().remove_dir_all(&external).await {
            lore_warn!(
                "Failed to remove external directory {} left after an abandoned FUSE clone: {error}",
                external.display()
            );
        }
        if created_mount_point
            && let Err(error) = lore_io::IoDriver::global()
                .remove_dir(repository_path)
                .await
            && error.kind() != std::io::ErrorKind::DirectoryNotEmpty
        {
            lore_warn!(
                "Unable to remove mount point {} left after an abandoned FUSE clone: {error}",
                repository_path.display()
            );
        }
        Ok(())
    }

    /// Unmounts every FUSE instance, as the service does when it stops.
    ///
    /// Without `force`, each mount gets a plain unmount, which the kernel refuses while anything
    /// holds the mount. If any is refused, the ones this call already unmounted are mounted
    /// again at the revision they served, so a refused stop leaves every instance mounted and
    /// served; derived inode numbers make the remount invisible to anything that looks up paths
    /// afresh. Busy paths are reported in `MountsBusy`. Asking first is not an option:
    /// `umount2(MNT_EXPIRE)` needs `CAP_SYS_ADMIN`, and a `/proc` scan cannot see other users'
    /// or root's processes and still races the unmount. With `force`, every mount is detached
    /// lazily and forgotten; nothing is remounted, and a path that stays attached is named in a
    /// warning while the stop goes ahead. From the snapshot of the mounts on, a mount another
    /// command makes is detached and fails, unless the stop is refused; once a stop goes ahead,
    /// always. A mount someone else unmounted or abandoned meanwhile is never mounted again, nor
    /// is one whose mount point a start would find missing (`InstanceStaleness::PathMissing`),
    /// which is forgotten rather than its mount point made again.
    ///
    /// SWFS mounts are not this manager's:
    /// [`SwfsMountManager::close_all`](crate::fs::swfs::mount_manager::SwfsMountManager::close_all)
    /// closes them once the service has stopped taking commands, where a command arriving after
    /// this could still use one.
    pub async fn unmount_all(&self, force: bool) -> Result<(), MountManagerError> {
        if force {
            self.exiting.store(true, Ordering::SeqCst);
        }
        self.stopping.store(true, Ordering::SeqCst);
        // Keyed as held: a path whose parent was renamed no longer resolves to its key.
        let fuse_mounts: Vec<(MountPath, Arc<FuseMount>)> = self
            .mounts
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        #[cfg(feature = "test-util")]
        self.stop_step(StopStep::Snapshot).await;
        let mut unmounted = Vec::new();
        let mut busy = Vec::new();
        let mut attached = Vec::new();
        let mut failure = None;
        for (mount_path, mount) in fuse_mounts {
            // A mount the map no longer holds was unmounted by someone else, as a move or another
            // stop does; its path may hold another mount now, which detaching by path would take.
            if !self.tracks_at(&mount_path, &mount) {
                continue;
            }
            match mount.unmount(force).await {
                Ok(()) => unmounted.push((mount_path, mount)),
                Err(error) if error.is_mounts_busy() => busy.push(mount.path.display().to_string()),
                // One the map dropped meanwhile counts as unmounted too: gone from its path is
                // what fails it, as when a move mounted the instance elsewhere under its name.
                Err(error) if !self.tracks_at(&mount_path, &mount) => {
                    if force {
                        lore_warn!(
                            "Stopping past {}, which another command unmounted meanwhile: {error}",
                            mount.path.display()
                        );
                    } else {
                        lore_debug!(
                            "Stopping past {}, which another command unmounted meanwhile: {error}",
                            mount.path.display()
                        );
                    }
                }
                Err(error) => {
                    attached.push(mount.path.display().to_string());
                    failure.get_or_insert(error);
                    if force {
                        self.mounts
                            .remove_if(&mount_path, |_, entry| Arc::ptr_eq(entry, &mount));
                    }
                }
            }
        }
        if force && let Some(error) = &failure {
            lore_warn!(
                "Stopping with FUSE mounts still attached at {}: {error}",
                attached.join(", ")
            );
        }
        #[cfg(feature = "test-util")]
        self.stop_step(StopStep::Unmounted).await;

        let refused = !force && (!busy.is_empty() || failure.is_some());
        for (mount_path, mount) in unmounted {
            if !refused {
                self.mounts
                    .remove_if(&mount_path, |_, entry| Arc::ptr_eq(entry, &mount));
                continue;
            }
            if !self.tracks_at(&mount_path, &mount) {
                continue;
            }
            // Forgotten by the test a start applies, rather than its mount point made again.
            if let Some(path) = mount.path.to_str()
                && instance_staleness(path, mount.instance_id).await
                    == InstanceStaleness::PathMissing
            {
                lore_info!(
                    "Forgetting FUSE instance {} whose mount point {path} is gone",
                    mount.instance_id
                );
                self.mounts
                    .remove_if(&mount_path, |_, entry| Arc::ptr_eq(entry, &mount));
                continue;
            }
            #[cfg(feature = "test-util")]
            self.stop_step(StopStep::Remounting).await;
            match Box::pin(self.in_service_context(mount.remount())).await {
                Ok(remounted) => {
                    let recorded = self
                        .mounts
                        .get_mut(&mount_path)
                        .filter(|entry| Arc::ptr_eq(entry.value(), &mount))
                        .map(|mut entry| {
                            *entry = remounted.clone();
                        })
                        .is_some();
                    // Another command unmounted or replaced it while it was mounted again.
                    if !recorded && let Err(error) = remounted.unmount(true).await {
                        lore_warn!(
                            "Unable to detach {}, remounted after a refused stop it no longer \
                             belongs to: {error}",
                            mount.path.display()
                        );
                    }
                }
                Err(error) => {
                    lore_warn!(
                        "Unable to remount {} after a refused stop: {error}",
                        mount.path.display()
                    );
                    self.mounts
                        .remove_if(&mount_path, |_, entry| Arc::ptr_eq(entry, &mount));
                }
            }
        }

        if !refused {
            self.exiting.store(true, Ordering::SeqCst);
            return Ok(());
        }
        self.stopping.store(false, Ordering::SeqCst);
        if !busy.is_empty() {
            return Err(MountsBusy {
                paths: busy.join(", "),
            }
            .into());
        }
        match failure {
            Some(error) => {
                Err(error).forward_any::<MountManagerError>("Failed to unmount FUSE instance")
            }
            None => Ok(()),
        }
    }

    /// Lets a manager whose stop went ahead mount again, for a test process that goes on using it.
    #[cfg(feature = "test-util")]
    pub fn resume_mounting(&self) {
        self.stopping.store(false, Ordering::SeqCst);
        self.exiting.store(false, Ordering::SeqCst);
    }

    /// Runs `hook` at each later [`StopStep`]. For tests; a second call panics.
    #[doc(hidden)]
    #[cfg(feature = "test-util")]
    pub fn set_stop_hook(&self, hook: StopHook) {
        assert!(
            self.stop_hook.set(hook).is_ok(),
            "a stop hook is already set"
        );
    }

    #[cfg(feature = "test-util")]
    async fn stop_step(&self, step: StopStep) {
        if let Some(hook) = self.stop_hook.get() {
            hook(step).await;
        }
    }

    async fn in_service_context<T>(&self, work: impl Future<Output = T>) -> T {
        match &self.context {
            Some(context) => LORE_CONTEXT.scope(context.clone(), work).await,
            None => work.await,
        }
    }

    /// [`Self::in_service_context`] for a mount the current call makes, with the environment
    /// the call was relayed with: the mount keeps the context it is built under for its life, so
    /// it reads the configuration and credentials its caller reads, as each relayed call does
    /// ([`lore_base::env`]). A mount restored when the service starts has no caller and serves
    /// with the service's own.
    async fn in_serving_context<T>(&self, work: impl Future<Output = T>) -> T {
        let environment =
            crate::runtime::try_execution_context().and_then(|call| call.environment().cloned());
        let service = self.context.clone().and_then(|context| {
            context
                .downcast::<crate::interface::ExecutionContext>()
                .ok()
        });
        match (service, environment) {
            (Some(service), Some(environment)) => {
                let serving: Arc<dyn Any + Send + Sync> = Arc::new(service.serving(environment));
                LORE_CONTEXT.scope(serving, work).await
            }
            _ => self.in_service_context(work).await,
        }
    }
}
