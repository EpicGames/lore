// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! One mounted FUSE instance: the kernel mount, the session serving it, and the provider
//! answering that session, plus the `FilesystemProvider` commands run against it.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::MountsBusy;
use lore_base::error::NotSupported;
use lore_error_set::prelude::*;
use lore_fuse::FuseError;
use lore_fuse::init::InitConfig;
use lore_fuse::metrics::Metrics;
use lore_fuse::mount::Mount;
use lore_fuse::mount::MountAccess;
use lore_fuse::mount::MountOptions;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::transport::device::DeviceTransport;
use lore_storage::FlushOptions;
use parking_lot::Mutex;

use crate::fs::filesystem_provider::CloneAnswer;
use crate::fs::filesystem_provider::CloneRun;
use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceCloneError;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
use crate::fs::fuse::access_recorder::AccessRecorder;
use crate::fs::fuse::attr::AttrContext;
use crate::fs::fuse::filesystem::FuseFilesystem;
use crate::fs::fuse::inode::Name;
use crate::fs::fuse::inode::ROOT_INO;
use crate::fs::fuse::notifications::Invalidations;
use crate::fs::fuse::operation::FuseOperation;
use crate::fs::mount_manager::MountRecord;
use crate::fs::walk::CloneStats;
use crate::instance::InstanceId;
use crate::lore_debug;
use crate::lore_warn;
use crate::repository::RepositoryContext;
use crate::repository::SharedRemote;
use crate::state::State;
use crate::util::path::RelativePath;

/// Largest READ the kernel may send, and the block size reported to it.
const MAX_READ: u32 = 1 << 20;

/// How long an unmount checking for leaks waits for the kernel's FORGETs.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// How long an unmount checking for leaks waits for the kernel's FORGETs before it invalidates
/// again.
const DRAIN_RESEND: Duration = Duration::from_millis(50);

/// How long the inode table must count no lookups before an unmount checking for leaks goes on.
const DRAIN_SETTLE: Duration = Duration::from_millis(100);

/// Leaked inode numbers named in the warning; the full list is in `leaked_lookups`.
const LEAKS_LOGGED: usize = 32;

#[error_set]
pub enum FuseMountError {
    MountsBusy,
}

pub struct FuseMount {
    pub path: PathBuf,
    pub instance_id: InstanceId,
    pub filesystem: Arc<FuseFilesystem>,
    session: Arc<Session<FuseFilesystem, DeviceTransport>>,
    // Keeps the device fd open for as long as this mount is served.
    _mount: Arc<Mount>,
    /// Inodes still holding lookups after the last plain unmount; with debug assertions only.
    leaked: Mutex<Option<Vec<u64>>>,
    /// The instance's external directory, with the global data directory it was resolved for;
    /// `None` for a read-only mount.
    external: Option<Arc<MountRecord>>,
}

impl FuseMount {
    /// Mounts `state` read-only at `path`, creating the mount point if needed. Call under the
    /// service's execution context: the provider keeps it for requests that arrive without one.
    pub async fn mount(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        path: &Path,
        instance_id: InstanceId,
        base_time: u64,
    ) -> Result<Arc<Self>, FuseMountError> {
        Self::mount_with(repository, state, path, instance_id, base_time, None).await
    }

    /// [`Self::mount`] read-write, over the local changes held in the instance's external
    /// directory, which `external` records ([`FuseFilesystem::new_writable`]), reaching the
    /// remote through `remote`, the instance's [`RepositoryContext::serving_remote`]. Make
    /// `remote` under the execution context of the call the mount serves, so its connect finds
    /// that caller's credentials, not the service's.
    pub(crate) async fn mount_writable(
        (repository, remote): (Arc<RepositoryContext>, SharedRemote),
        state: Arc<State>,
        path: &Path,
        instance_id: InstanceId,
        base_time: u64,
        external: Arc<MountRecord>,
    ) -> Result<Arc<Self>, FuseMountError> {
        Self::mount_with(
            repository,
            state,
            path,
            instance_id,
            base_time,
            Some((external, remote)),
        )
        .await
    }

    async fn mount_with(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        path: &Path,
        instance_id: InstanceId,
        base_time: u64,
        writable: Option<(Arc<MountRecord>, SharedRemote)>,
    ) -> Result<Arc<Self>, FuseMountError> {
        let attr = AttrContext::for_mounting_user(MAX_READ, base_time);
        let (filesystem, external) = match writable {
            Some((record, remote)) => (
                FuseFilesystem::new_writable_over(
                    (repository, remote),
                    state,
                    attr,
                    &record.external_dir,
                )
                .await,
                Some(record),
            ),
            None => (FuseFilesystem::new(repository, state, attr).await, None),
        };
        let filesystem = filesystem.forward_any::<FuseMountError>("building the FUSE provider")?;
        if let Some(recorder) = AccessRecorder::from_env(&instance_id.text_encoding()) {
            filesystem.start_access_recording(recorder);
        }

        let (access, init) = match external {
            Some(_) => (MountAccess::ReadWrite, InitConfig::read_write()),
            None => (MountAccess::ReadOnly, InitConfig::default()),
        };
        let options = MountOptions {
            fsname: fsname(instance_id),
            max_read: MAX_READ,
            fusermount: None,
        };
        let mount_path = path.to_path_buf();
        let mount = lore_base::lore_spawn_blocking!(move || {
            std::fs::create_dir_all(&mount_path)
                .internal_with(|| format!("creating mount point {}", mount_path.display()))
                .map_err(FuseMountError::from)?;
            lore_fuse::mount::mount_with_access(&mount_path, &options, access)
                .internal("mounting the instance")
                .map_err(FuseMountError::from)
        })
        .await
        .internal("mount task panicked")??;
        let mount = Arc::new(mount);

        let config = SessionConfig {
            init: InitConfig {
                zero_message_open: true,
                ..init
            },
            ..SessionConfig::default()
        };
        let transport = Arc::new(DeviceTransport::new(mount.clone()));
        let started = match Session::start(filesystem.clone(), transport, config).await {
            Ok(session) => session.notifier().map(|notifier| (session, notifier)),
            Err(error) => Err(error),
        };
        let session = match started {
            Ok((session, notifier)) => match filesystem.set_notifier(notifier) {
                Ok(()) => session,
                Err(error) => {
                    session.shutdown().await;
                    let _ = lore_fuse::mount::unmount_path_on_blocking_thread(path, true).await;
                    return Err(FuseMountError::internal(format!(
                        "starting the notification thread: {error}"
                    )));
                }
            },
            Err(error) => {
                let _ = lore_fuse::mount::unmount_path_on_blocking_thread(path, true).await;
                return Err(FuseMountError::internal_with_context(
                    error,
                    "starting the FUSE session",
                ));
            }
        };

        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            instance_id,
            filesystem,
            session,
            _mount: mount,
            leaked: Mutex::new(None),
            external,
        }))
    }

    /// Unmounts and stops serving, as [`Self::unmount_retrying_busy_since`] does with the busy
    /// window starting now.
    pub fn unmount(&self, lazy: bool) -> impl Future<Output = Result<(), FuseMountError>> + '_ {
        self.unmount_retrying_busy_since(lazy, Instant::now())
    }

    /// Unmounts and stops serving. A plain unmount retries a busy mount until its busy window
    /// after `since` ends, as [`lore_fuse::mount::unmount_path_retrying_busy`] does; a mount still
    /// busy then fails with `MountsBusy` and stays mounted and served. A lazy unmount detaches the
    /// mount whatever holds it. A mount already gone from the mount table, detached by someone
    /// else, counts as unmounted. Once the session has stopped, the background prefetch is
    /// cancelled; nothing is left to record, as every request changed the filesystem State and
    /// `write/` before it was answered, and the notification thread sends what is queued and ends.
    /// With debug assertions a plain unmount first drains the kernel's lookups, which its busy
    /// window does not count, then records and warns about any the table still counts.
    pub async fn unmount_retrying_busy_since(
        &self,
        lazy: bool,
        since: Instant,
    ) -> Result<(), FuseMountError> {
        let check_leaks = cfg!(debug_assertions) && !lazy;
        let since = if check_leaks {
            let draining = Instant::now();
            self.drain_kernel_lookups().await;
            since + draining.elapsed()
        } else {
            since
        };
        let unmounted = if lazy {
            lore_fuse::mount::unmount_path_on_blocking_thread(&self.path, true).await
        } else {
            lore_fuse::mount::unmount_path_retrying_busy(&self.path, since).await
        };
        match unmounted {
            Ok(()) => {}
            Err(FuseError::UnmountBusy(_)) if !lazy => {
                return Err(MountsBusy {
                    paths: self.path.display().to_string(),
                }
                .into());
            }
            Err(error) if self.is_attached().await => {
                return Err(FuseMountError::internal_with_context(
                    error,
                    "unmounting the instance",
                ));
            }
            Err(_detached_elsewhere) => {}
        }
        self.session.shutdown().await;
        self.filesystem.stop_notifications().await;
        self.filesystem.stop_prefetch();
        // What READs and prefetches fetched is in the local store flagged durable, which
        // schedules no index flush: unflushed, a service started later finds none of it and
        // fetches it all again.
        if let Err(error) = self
            .filesystem
            .served()
            .repository
            .flush(FlushOptions::default())
            .await
        {
            lore_warn!(
                "FUSE mount {} could not flush its stores: {error}",
                self.path.display()
            );
        }
        lore_debug!(
            "FUSE mount {} unmounted with {} bytes of chain logs",
            self.path.display(),
            self.filesystem.chain_log_bytes()
        );
        if check_leaks {
            self.check_leaks();
        }
        Ok(())
    }

    /// Whether the mount table still holds a mount of this instance, wherever it is listed.
    async fn is_attached(&self) -> bool {
        let source = fsname(self.instance_id);
        lore_base::lore_spawn_blocking!(move || lore_fuse::mount::is_mounted(&source))
            .await
            .unwrap_or(true)
    }

    /// Makes the kernel forget every inode it holds, so the table can be checked against it:
    /// once the superblock is torn down the kernel sends no FORGET for what it still holds.
    /// Invalidating the root's children prunes every dentry the kernel reaches through them. Two
    /// kinds outlast that: another process, such as a desktop's file monitor, can look a name up
    /// again, and the kernel keeps the dentries below a directory whose name it dropped while a
    /// file below it was open, which no path from the root reaches. So while the table counts
    /// lookups, the invalidations go out again every [`DRAIN_RESEND`], with the children of every
    /// directory that holds a looked-up inode ([`Self::drain_names`]). The drain ends once the
    /// table has counted none for [`DRAIN_SETTLE`], or [`DRAIN_WAIT`] after its first
    /// invalidations. A lookup made after it ends and before the umount still counts as leaked.
    #[lore_macro::test_pub]
    async fn drain_kernel_lookups(&self) {
        let Some(queue) = self.filesystem.notifications() else {
            return;
        };
        let (mut entries, mut live) = (Vec::new(), Vec::new());
        let mut deadline = None;
        let mut resend = Instant::now();
        let mut drained_since = None;
        loop {
            let now = Instant::now();
            if self.filesystem.inodes().check_all_zero().is_ok() {
                if drained_since.is_none() {
                    #[cfg(feature = "test-util")]
                    crate::test_util::unmount_drain_emptied().await;
                }
                if now - *drained_since.get_or_insert(now) >= DRAIN_SETTLE {
                    return;
                }
            } else {
                drained_since = None;
            }
            if deadline.is_some_and(|deadline| now >= deadline) {
                return;
            }
            if drained_since.is_none() && now >= resend {
                self.drain_names(deadline.is_some(), &mut entries, &mut live);
                entries = queue
                    .send(Invalidations {
                        entries,
                        inodes: Vec::new(),
                    })
                    .await
                    .entries;
                resend = Instant::now() + DRAIN_RESEND;
                deadline.get_or_insert_with(|| Instant::now() + DRAIN_WAIT);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Fills `entries` with the names the drain invalidates, each paired with its directory: the
    /// root's children, and with `below_root` the children of every directory that holds an
    /// inode the table counts lookups of, each by every spelling the kernel may cache it under.
    /// The kernel finds a child's dentry through its directory's inode rather than its name, so
    /// the names below the root reach the dentries below a directory whose name it dropped.
    /// `live` is scratch.
    fn drain_names(&self, below_root: bool, entries: &mut Vec<(u64, Name)>, live: &mut Vec<u64>) {
        entries.clear();
        live.clear();
        self.filesystem.inodes().live(live);
        for &ino in live.iter() {
            if let Some(entry) = self.filesystem.inodes().get(ino)
                && entry.ino != ROOT_INO
                && (below_root || entry.parent == ROOT_INO)
            {
                entries.push((entry.parent, entry.name.clone()));
            }
        }
        live.clear();
        live.push(ROOT_INO);
        live.extend(entries.iter().map(|&(parent, _)| parent));
        live.sort_unstable();
        live.dedup();
        self.filesystem.spellings().copy_children_of(live, entries);
        entries.sort_unstable();
        entries.dedup();
    }

    fn check_leaks(&self) {
        let leaked = match self.filesystem.inodes().check_all_zero() {
            Ok(()) => Vec::new(),
            Err(leaked) => {
                lore_warn!(
                    "FUSE mount {} unmounted with lookups outstanding on {} inodes, first {:?}",
                    self.path.display(),
                    leaked.len(),
                    &leaked[..leaked.len().min(LEAKS_LOGGED)]
                );
                leaked
            }
        };
        *self.leaked.lock() = Some(leaked);
    }

    /// The inodes that still held lookups when the last plain unmount finished, in builds with
    /// debug assertions; `None` before that, after a lazy unmount, and without them.
    pub fn leaked_lookups(&self) -> Option<Vec<u64>> {
        self.leaked.lock().clone()
    }

    /// [`Self::remount`] at `path`, served through `repository`, the instance's context rooted
    /// there: what moving the instance's mount point mounts. Inode numbers derive from paths below
    /// the mount point, so the instance answers with the numbers it had.
    pub async fn remount_at(
        &self,
        path: &Path,
        repository: Arc<RepositoryContext>,
    ) -> Result<Arc<Self>, FuseMountError> {
        Self::mount_with(
            repository,
            self.filesystem.served().state.clone(),
            path,
            self.instance_id,
            self.filesystem.base_time(),
            self.writable(),
        )
        .await
    }

    /// The external directory of a read-write mount, with the remote it reaches, which mounting
    /// the instance again keeps: made for the call that first mounted it, it holds that caller's
    /// credentials. `None` for a read-only mount.
    fn writable(&self) -> Option<(Arc<MountRecord>, SharedRemote)> {
        self.external
            .clone()
            .map(|record| (record, self.filesystem.remote()))
    }

    /// The external directory of a read-write mount, with the global data directory it was
    /// resolved for. `None` for a read-only mount.
    pub fn record(&self) -> Option<&Arc<MountRecord>> {
        self.external.as_ref()
    }

    /// Mounts this instance again, after it was unmounted, at the revision it last served and
    /// with the access it had.
    pub async fn remount(&self) -> Result<Arc<Self>, FuseMountError> {
        let served = self.filesystem.served();
        Self::mount_with(
            served.repository.clone(),
            served.state.clone(),
            &self.path,
            self.instance_id,
            self.filesystem.base_time(),
            self.writable(),
        )
        .await
    }

    pub fn metrics(&self) -> &Metrics {
        self.session.metrics()
    }
}

/// The source an instance's mount carries in the mount table.
fn fsname(instance_id: InstanceId) -> String {
    format!("lore:{}", instance_id.text_encoding())
}

pub struct FuseFilesystemProvider {
    mount: Arc<FuseMount>,
}

impl FuseFilesystemProvider {
    pub fn new(mount: Arc<FuseMount>) -> Self {
        Self { mount }
    }
}

#[async_trait]
impl FilesystemProvider for FuseFilesystemProvider {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(
                self.mount.filesystem.clone(),
            )),
            self.holds_state(),
        )))
    }

    fn holds_state(&self) -> bool {
        true
    }

    async fn read_held_file(&self, path: &RelativePath) -> Result<Option<Bytes>, FsError> {
        self.mount
            .filesystem
            .read_held_file(path)
            .await
            .forward_any::<FsError>("reading a file the mount serves")
    }

    /// What cloning the mount's instance does: its external directory is cloned beside it, as
    /// [`crate::fs::fuse::clone::answer`] probes, whatever the destination, which only takes the
    /// new instance's mount. A read-only mount, which has no external directory, cannot be cloned.
    async fn clone_answer(
        &self,
        _source: &Path,
        _dot_path: &Path,
        _destination: &Path,
    ) -> Result<CloneAnswer, InstanceCloneError> {
        match &self.mount.external {
            Some(external) => crate::fs::fuse::clone::answer(&external.external_dir).await,
            None => Ok(CloneAnswer::Unsupported {
                reason: "a read-only mount cannot be cloned".to_string(),
            }),
        }
    }

    /// Clones the mount's external directory into `destination`, the new instance's external
    /// directory, as [`crate::fs::fuse::clone::clone`] does.
    async fn clone_instance(
        &self,
        _source: &Path,
        _dot_path: &Path,
        destination: &Path,
        answer: &CloneAnswer,
        copy_unclonable: bool,
        run: &CloneRun,
    ) -> Result<CloneStats, InstanceCloneError> {
        let Some(external) = self
            .mount
            .external
            .as_ref()
            .map(|record| &record.external_dir)
        else {
            return Err(NotSupported {
                operation: "cloning a read-only mount".to_string(),
            }
            .into());
        };
        crate::fs::fuse::clone::clone(external, destination, answer, copy_unclonable, run).await
    }
}
