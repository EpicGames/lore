// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Stopping the Lore service for a move of the external root: the status read first, a handle on
//! the service's process, the stop, the socket held, the wait for the process and for exclusive
//! access to the root, and the start once the move ends.
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::InvalidArguments;
use lore_base::error::MountsBusy;
use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::event::LoreServiceStatusEventData;
use lore_revision::global::external_dir::access::try_exclusive_for_move;
use lore_revision::global::external_dir::canonical_global;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore_warn;
use parking_lot::Mutex;
use tokio::time::Instant;

use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::remote::call::Relayed;
use crate::remote::call::run_service_call;
use crate::remote::command::LoreCommand;
use crate::remote::network::UdsStream;
use crate::remote::occupy::OccupiedSocket;
use crate::remote::occupy::READ_TIMEOUT;
use crate::remote::service_process::Reached;
use crate::remote::service_process::ServiceProcessError;
use crate::remote::service_process::reach_running_service;
use crate::remote::service_process::spawn_service_and_connect;
use crate::remote::service_socket_name;
use crate::service::LoreServiceStatusArgs;
use crate::service::LoreServiceStopArgs;

/// How long a move waits, from the stop, for the stopped service to give up its socket, for its
/// process to exit and for its shared access to the root to be released. It covers the service's
/// own shutdown, about 35 seconds at most, its SWFS mounts closing, and Windows releasing a file
/// lock just after its holder exits.
pub const WAIT_BOUND: Duration = Duration::from_secs(60);

/// How often the wait tries the socket and the lock again.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[error_set]
pub enum StopForMoveError {
    InvalidArguments,
    MountsBusy,
}

impl EventError for StopForMoveError {}

/// What answers on the service socket before a move.
#[allow(clippy::large_enum_variant)]
pub enum FoundService {
    /// Nothing answers.
    None,
    /// A service that reports its status.
    Reporting(LoreServiceStatusEventData),
    /// A service whose status can't be read, as a service of another build answers.
    OtherBuild,
    /// Another move of the external root holds the socket.
    Moving,
}

/// The service a move stopped, with the socket and the access the move holds until it ends.
pub struct Stopped {
    socket: OccupiedSocket,
    access: FSLock,
    restart: Option<PathBuf>,
}

/// What became of the service once a move ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// No service was stopped that could be started again.
    NoneStopped,
    /// A service answers again.
    Started,
    /// The service couldn't be started again, which a warning names.
    Failed,
    /// The service was started again, and the wait for it to answer was cancelled.
    Unawaited,
}

/// The connection to the service of another process that listens on the socket, if any, as
/// [`reach_running_service`] reaches it. Refuses with [`service_of_this_program`] where the
/// service runs in this process.
async fn reach_service() -> Result<Option<UdsStream>, StopForMoveError> {
    match reach_running_service()
        .await
        .forward_any::<StopForMoveError>("connecting to the Lore service")?
    {
        Reached::Here => Err(service_of_this_program().into()),
        Reached::Connected(connection) => Ok(Some(connection)),
        Reached::Nothing => Ok(None),
    }
}

/// The refusal of a move by a program that runs the service, which the move can't stop and
/// start again.
pub(crate) fn service_of_this_program() -> MountsBusy {
    MountsBusy {
        paths: "the Lore service this program runs, which a move can't stop and start again"
            .to_string(),
    }
}

/// Reads the status of the service that answers on the socket. A reply this build can't read, or
/// a success that brings no status of a running service, is one of another build; any other
/// failure fails. Refuses with [`service_of_this_program`] where that service runs in this
/// process.
pub async fn find_service() -> Result<FoundService, StopForMoveError> {
    let connection = match reach_service().await? {
        Some(connection) => connection,
        None => return Ok(FoundService::None),
    };
    let found: Arc<Mutex<(Option<LoreServiceStatusEventData>, String)>> = Arc::default();
    let callback: LoreEventCallback = Some(Box::new({
        let found = Arc::clone(&found);
        move |event: &LoreEvent| match event {
            LoreEvent::ServiceStatus(status) => found.lock().0 = Some(status.clone()),
            LoreEvent::Complete(complete) if complete.status != 0 => {
                complete
                    .error
                    .message
                    .as_str()
                    .clone_into(&mut found.lock().1);
            }
            _ => {}
        }
    }));
    let relayed = relay_quietly(
        connection,
        LoreGlobalArgs::default(),
        LoreServiceStatusArgs {}.into(),
        callback,
    )
    .await;
    let (status, failure) = std::mem::take(&mut *found.lock());
    match (relayed, status) {
        (Relayed::HandedBack(_), _) => Ok(FoundService::Moving),
        (Relayed::Ran(0), Some(status)) if status.running != 0 => {
            Ok(FoundService::Reporting(status))
        }
        (Relayed::Ran(0), _) => Ok(FoundService::OtherBuild),
        (Relayed::Ran(_), _) if shows_another_build(&failure) => Ok(FoundService::OtherBuild),
        (Relayed::Ran(_), _) => Err(StopForMoveError::internal(format!(
            "reading the Lore service's status: {failure}"
        ))),
    }
}

/// Whether the failure `failure` of a call is one a service of another build causes: a reply of
/// another protocol version, or one this build can't decode.
fn shows_another_build(failure: &str) -> bool {
    failure.contains(crate::remote::message::WRONG_VERSION)
        || failure.contains(crate::remote::message::DECODING_MESSAGE)
}

/// Refuses to stop the service `status` reports for a move of the external root of `global`, a
/// canonical path, to `target`: one running inside another program, one that mounts virtual
/// instances of another global data directory, and one that mounts a virtual instance that
/// `target` lies inside.
#[lore_macro::test_pub]
pub(crate) fn refuse_reported_service(
    status: &LoreServiceStatusEventData,
    global: &Path,
    target: &Path,
) -> Result<(), StopForMoveError> {
    if status.embedded != 0 {
        return Err(MountsBusy {
            paths: format!(
                "the Lore service runs inside another program (process {}), which a move can't \
                 start again",
                status.process_id
            ),
        }
        .into());
    }
    if let Some(other) = status
        .mounted_global_data_directories
        .as_slice()
        .iter()
        .find(|mounted| Path::new(mounted.as_str()) != global)
    {
        return Err(MountsBusy {
            paths: format!(
                "the Lore service mounts virtual instances of the global data directory {other}"
            ),
        }
        .into());
    }
    if let Some(mounted) = status
        .mounted_paths
        .as_slice()
        .iter()
        .find(|mounted| target.starts_with(mounted.as_str()))
    {
        return Err(InvalidArguments {
            reason: format!(
                "{} lies inside the virtual instance at {mounted}",
                target.display()
            ),
        }
        .into());
    }
    Ok(())
}

/// Stops the service `found` reports, as `lore service stop` does, before a move of the external
/// root of `global` to `target`, then holds the socket and takes exclusive access to the root once
/// the stopped service's process has exited, waiting at most `bound` from the stop. Where a
/// service that reported a process ID was asked to stop here, it is started again on a refusal
/// after the stop, except where another service holds the socket, and where `termination`
/// completes before the socket and the access are held, which fails the stop.
pub async fn stop_for_move(
    global: &Path,
    target: &Path,
    found: FoundService,
    force: bool,
    bound: Duration,
    termination: impl Future<Output = ()>,
) -> Result<Stopped, StopForMoveError> {
    let global = canonical_global(global)
        .forward::<StopForMoveError>("finding the global data directory")?;
    let mut asked = None;
    let stopped = tokio::select! {
        stopped = stop_and_hold(&global, target, found, force, bound, &mut asked) => Some(stopped),
        () = termination => None,
    };
    if let Some(stopped) = stopped {
        return stopped;
    }
    if let Some(executable) = asked {
        start_again(&executable).await;
    }
    Err(StopForMoveError::internal(
        "the move of the external root was cancelled before the Lore service stopped",
    ))
}

/// The steps of [`stop_for_move`] that `termination` can cancel. Records in `asked` the
/// executable of the service it asks to stop, which reported a process ID.
async fn stop_and_hold(
    global: &Path,
    target: &Path,
    found: FoundService,
    force: bool,
    bound: Duration,
    asked: &mut Option<PathBuf>,
) -> Result<Stopped, StopForMoveError> {
    let (mut process, restart) = match found {
        FoundService::None => (None, None),
        FoundService::Moving => return Err(socket_held().into()),
        FoundService::OtherBuild => {
            stop_service(force).await?;
            (None, None)
        }
        FoundService::Reporting(status) => {
            refuse_reported_service(&status, global, target)?;
            let process = (status.process_id != 0)
                .then(|| ServiceProcess::open(status.process_id))
                .transpose()
                .internal("opening the Lore service's process")?;
            let restart = process
                .is_some()
                .then(|| PathBuf::from(status.binary_path.as_str()));
            asked.clone_from(&restart);
            stop_service(force).await?;
            (process, restart)
        }
    };
    let deadline = Instant::now() + bound;
    let socket = occupy_socket(service_socket_name(), process.as_mut(), deadline).await?;
    match take_exclusive_access(global, process.as_mut(), deadline).await {
        Ok(access) => Ok(Stopped {
            socket,
            access,
            restart,
        }),
        Err(error) => {
            socket.release().await;
            if let Some(executable) = restart {
                start_again(&executable).await;
            }
            Err(error)
        }
    }
}

impl Stopped {
    /// The exclusive access to the external root the move holds.
    pub fn access(&self) -> &FSLock {
        &self.access
    }

    /// Gives up exclusive access and the socket, and starts the stopped service again from its
    /// executable, where it reported a process ID, waiting for it to answer until `termination`.
    pub async fn end(self, termination: impl Future<Output = ()>) -> Restart {
        let Self {
            socket,
            access,
            restart,
        } = self;
        drop(access);
        socket.release().await;
        let Some(executable) = restart else {
            return Restart::NoneStopped;
        };
        tokio::select! {
            started = start_again(&executable) => {
                if started { Restart::Started } else { Restart::Failed }
            }
            () = termination => Restart::Unawaited,
        }
    }
}

/// Asks the service that answers on the socket to stop, detaching busy FUSE mounts lazily where
/// `force` is set, as `lore service stop` does. Fails with `MountsBusy` where a busy mount refuses
/// the stop.
async fn stop_service(force: bool) -> Result<(), StopForMoveError> {
    let Some(connection) = reach_service().await? else {
        return Ok(());
    };
    let failure: Arc<Mutex<Option<(i32, String)>>> = Arc::default();
    let callback: LoreEventCallback = Some(Box::new({
        let failure = Arc::clone(&failure);
        move |event: &LoreEvent| {
            if let LoreEvent::Complete(complete) = event
                && complete.status != 0
            {
                *failure.lock() = Some((
                    complete.error.error_code,
                    complete.error.message.as_str().to_string(),
                ));
            }
        }
    }));
    let globals = LoreGlobalArgs {
        force: u8::from(force),
        ..LoreGlobalArgs::default()
    };
    let status =
        match relay_quietly(connection, globals, LoreServiceStopArgs {}.into(), callback).await {
            Relayed::Ran(status) => status,
            Relayed::HandedBack(_) => return Err(socket_held().into()),
        };
    if status == 0 {
        return Ok(());
    }
    let (code, message) = failure
        .lock()
        .take()
        .unwrap_or_else(|| (status, String::new()));
    if code == MountsBusy::FFI_CODE {
        let paths = message
            .strip_prefix("Mounts are busy: ")
            .unwrap_or(&message);
        return Err(MountsBusy {
            paths: paths.to_string(),
        }
        .into());
    }
    Err(StopForMoveError::internal(format!(
        "the Lore service didn't stop: {message}"
    )))
}

/// Runs `command` on the service over `connection`, with `callback` handed its events, from an
/// execution of its own, so that none of its events reach the caller's callback.
async fn relay_quietly(
    connection: UdsStream,
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> Relayed {
    let relay = run_service_call(Some(connection), globals, command, callback);
    let quiet = crate::call::setup_execution(LoreGlobalArgs::default(), None);
    LORE_CONTEXT.scope(quiet, relay).await
}

/// The refusal of a move while another process holds the service socket.
pub(crate) fn socket_held() -> MountsBusy {
    MountsBusy {
        paths: format!("the Lore service socket {}", service_socket_name()),
    }
}

/// Holds the socket `name` names as soon as the stopped service gives it up, trying every
/// [`POLL_INTERVAL`] until `deadline`. Refuses with `MountsBusy` where the claim still fails once
/// `process` has exited, which means another service holds the socket, or at `deadline`. With no
/// process, as for a service that reports none, tries until `deadline`.
#[lore_macro::test_pub]
async fn occupy_socket(
    name: &str,
    mut process: Option<&mut ServiceProcess>,
    deadline: Instant,
) -> Result<OccupiedSocket, StopForMoveError> {
    loop {
        let exited = match process.as_deref_mut() {
            Some(process) => process
                .has_exited()
                .internal("waiting for the Lore service to exit")?,
            None => false,
        };
        let name_owned = name.to_string();
        let claimed = lore_base::lore_spawn_blocking!(move || OccupiedSocket::claim(
            &name_owned,
            READ_TIMEOUT
        ))
        .await
        .internal("joining the claim of the service socket")?
        .forward::<StopForMoveError>("claiming the service socket")?;
        if let Some(socket) = claimed {
            return Ok(socket);
        }
        if exited || Instant::now() >= deadline {
            return Err(MountsBusy {
                paths: format!("the Lore service socket {name}"),
            }
            .into());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Takes exclusive access to the external root of `global` once `process` has exited and its
/// shared access is released, trying every [`POLL_INTERVAL`] until `deadline`, then refuses with
/// `MountsBusy` naming what is still there. With no process, waits for the lock alone.
#[lore_macro::test_pub]
async fn take_exclusive_access(
    global: &Path,
    mut process: Option<&mut ServiceProcess>,
    deadline: Instant,
) -> Result<FSLock, StopForMoveError> {
    let mut access = None;
    loop {
        let exited = match process.as_deref_mut() {
            Some(process) => process
                .has_exited()
                .internal("waiting for the Lore service to exit")?,
            None => true,
        };
        if access.is_none() {
            let global = global.to_path_buf();
            access = lore_base::lore_spawn_blocking!(move || try_exclusive_for_move(&global))
                .await
                .internal("joining the try of exclusive access")?
                .forward::<StopForMoveError>("taking exclusive access to the external root")?;
        }
        if exited && let Some(access) = access {
            return Ok(access);
        }
        if Instant::now() >= deadline {
            let held = if access.is_none() {
                format!("external.lock in {}", global.display())
            } else {
                "the stopped Lore service, whose process is still running".to_string()
            };
            return Err(MountsBusy { paths: held }.into());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Starts a service again from `executable`, once more where the first start fails, and answers
/// whether one answers. A second failure is reported as a warning naming the executable.
pub async fn start_again(executable: &Path) -> bool {
    match start_twice(|| spawn_service_and_connect(executable.to_path_buf())).await {
        Ok(_) => true,
        Err(error) => {
            lore_warn!(
                "The Lore service couldn't be started again from {}: {error}. The next command \
                 that needs it starts one",
                executable.display()
            );
            false
        }
    }
}

/// Runs `start`, and once more where it fails, answering the last result.
#[lore_macro::test_pub]
async fn start_twice<Started, Starting>(
    mut start: impl FnMut() -> Starting,
) -> Result<Started, ServiceProcessError>
where
    Starting: Future<Output = Result<Started, ServiceProcessError>>,
{
    match start().await {
        Ok(started) => Ok(started),
        Err(_) => start().await,
    }
}

/// A handle on a process, opened by its ID, that tells when the process has exited. Opened while
/// the process runs, it can't mistake a later process given the same ID for it.
pub struct ServiceProcess {
    platform: platform::Handle,
}

impl ServiceProcess {
    /// Opens a handle on the process `process_id` names. A process that has already exited opens
    /// as one that has.
    pub fn open(process_id: u32) -> std::io::Result<ServiceProcess> {
        Ok(ServiceProcess {
            platform: platform::Handle::open(process_id)?,
        })
    }

    /// Whether the process has exited.
    pub fn has_exited(&mut self) -> std::io::Result<bool> {
        self.platform.has_exited()
    }

    /// A handle on the process `process_id` that asks whether it exists at each check, as
    /// [`Self::open`] makes one where the kernel refuses a pidfd.
    #[cfg(all(target_os = "linux", feature = "test-util"))]
    pub fn polling(process_id: u32) -> std::io::Result<ServiceProcess> {
        Ok(ServiceProcess {
            platform: platform::Handle::polling(process_id)?,
        })
    }
}

/// The process handle on Linux: a pidfd, which turns readable once the process exits, or where
/// the kernel refuses one, as a kernel before 5.3 or a seccomp filter does, the process ID, whose
/// process is asked for at each check. A process ID can't tell a process from a later one that
/// reuses its ID.
#[cfg(target_os = "linux")]
mod platform {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    /// What the handle watches.
    pub(super) enum Handle {
        /// The pidfd.
        Pidfd(OwnedFd),
        /// The process ID, where the kernel refused a pidfd.
        Polled(libc::pid_t),
        /// Nothing, for a process that had exited before it was opened.
        Exited,
    }

    impl Handle {
        pub(super) fn open(process_id: u32) -> std::io::Result<Handle> {
            let process_id = libc::pid_t::try_from(process_id).map_err(std::io::Error::other)?;
            // SAFETY: `pidfd_open` takes a process ID and flags, and reads no memory.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process_id, 0) };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ESRCH) => Ok(Handle::Exited),
                    Some(libc::ENOSYS | libc::EPERM) => Ok(Handle::Polled(process_id)),
                    _ => Err(error),
                };
            }
            let fd = i32::try_from(fd).map_err(std::io::Error::other)?;
            // SAFETY: `pidfd_open` returned the descriptor, which nothing else owns.
            Ok(Handle::Pidfd(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        #[cfg(feature = "test-util")]
        pub(super) fn polling(process_id: u32) -> std::io::Result<Handle> {
            libc::pid_t::try_from(process_id)
                .map(Handle::Polled)
                .map_err(std::io::Error::other)
        }

        pub(super) fn has_exited(&mut self) -> std::io::Result<bool> {
            match self {
                Handle::Pidfd(pidfd) => {
                    let mut poll = libc::pollfd {
                        fd: pidfd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: polls one descriptor this owns, through a `pollfd` that outlives
                    // the call, without waiting.
                    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
                    if ready < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(ready > 0)
                }
                Handle::Polled(process_id) => {
                    // SAFETY: signal 0 sends nothing, and `kill` reads no memory.
                    if unsafe { libc::kill(*process_id, 0) } == 0 {
                        return Ok(false);
                    }
                    let error = std::io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::ESRCH) => Ok(true),
                        Some(libc::EPERM) => Ok(false),
                        _ => Err(error),
                    }
                }
                Handle::Exited => Ok(true),
            }
        }
    }
}

/// The process handle on macOS: a kqueue holding an `EVFILT_PROC` filter for `NOTE_EXIT`.
#[cfg(target_os = "macos")]
mod platform {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    /// The kqueue, or `None` once the process is known to have exited.
    pub(super) struct Handle {
        kqueue: Option<OwnedFd>,
    }

    impl Handle {
        pub(super) fn open(process_id: u32) -> std::io::Result<Handle> {
            // SAFETY: `kqueue` takes no arguments.
            let kqueue = unsafe { libc::kqueue() };
            if kqueue < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `kqueue` returned the descriptor, which nothing else owns.
            let kqueue = unsafe { OwnedFd::from_raw_fd(kqueue) };
            let filter = libc::kevent {
                ident: process_id as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: registers one filter, read from a `kevent` that outlives the call, and
            // asks for no events back.
            let added = unsafe {
                libc::kevent(
                    kqueue.as_raw_fd(),
                    std::ptr::from_ref(&filter),
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if added < 0 {
                let error = std::io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ESRCH) => Ok(Handle { kqueue: None }),
                    _ => Err(error),
                };
            }
            Ok(Handle {
                kqueue: Some(kqueue),
            })
        }

        pub(super) fn has_exited(&mut self) -> std::io::Result<bool> {
            let Some(kqueue) = &self.kqueue else {
                return Ok(true);
            };
            // SAFETY: a `kevent` of integers and a null pointer, which zeroed memory is.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: reads at most one event into `event`, without waiting, from a kqueue this
            // owns.
            let ready = unsafe {
                libc::kevent(
                    kqueue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    std::ptr::from_mut(&mut event),
                    1,
                    std::ptr::from_ref(&now),
                )
            };
            if ready < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if ready > 0 {
                self.kqueue = None;
            }
            Ok(ready > 0)
        }
    }
}

/// The process handle on Windows: a process handle opened with `SYNCHRONIZE`, which is signaled
/// once the process exits.
#[cfg(target_os = "windows")]
mod platform {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;

    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::WAIT_FAILED;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// The process handle, or `None` for a process that had exited before it was opened.
    pub(super) struct Handle {
        process: Option<OwnedHandle>,
    }

    impl Handle {
        pub(super) fn open(process_id: u32) -> std::io::Result<Handle> {
            // SAFETY: takes a process ID and access rights, and reads no memory.
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
            if process.is_null() {
                let error = std::io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(code) if code.cast_unsigned() == ERROR_INVALID_PARAMETER => {
                        Ok(Handle { process: None })
                    }
                    _ => Err(error),
                };
            }
            // SAFETY: `OpenProcess` returned the handle, which nothing else owns.
            Ok(Handle {
                process: Some(unsafe { OwnedHandle::from_raw_handle(process) }),
            })
        }

        pub(super) fn has_exited(&mut self) -> std::io::Result<bool> {
            let Some(process) = &self.process else {
                return Ok(true);
            };
            // SAFETY: waits on a handle this owns, without waiting.
            let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) };
            if waited == WAIT_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            Ok(waited == WAIT_OBJECT_0)
        }
    }
}

/// No process handle elsewhere: a process reads as running until the wait's bound.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    pub(super) struct Handle;

    impl Handle {
        pub(super) fn open(_process_id: u32) -> std::io::Result<Handle> {
            Ok(Handle)
        }

        pub(super) fn has_exited(&mut self) -> std::io::Result<bool> {
            Ok(false)
        }
    }
}
