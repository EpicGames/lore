// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service process a call is served by: connecting to the one that runs,
//! starting one when none does, and stopping the one that runs in this process.
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::error::ServiceUnavailable;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::global::GlobalConfig;
use lore_revision::interface::LoreError;
use lore_revision::lore_debug;
use lore_revision::lore_warn;
use lore_revision::util::config::SaveableConfig;
use parking_lot::Mutex;
use parking_lot::RwLock;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::remote::network::UdsStream;
use crate::remote::network::uds_supported;
use crate::remote::service_socket_name;

#[error_set]
pub enum ServiceProcessError {
    ServiceUnavailable,
}

impl EventError for ServiceProcessError {
    fn translated(&self) -> LoreError {
        match self {
            // The one failure here that means the call did not run, which is
            // what a caller deciding whether to run it itself has to tell apart.
            Self::ServiceUnavailable(_) => LoreError::ServiceUnavailable,
            Self::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Reports that no service could be reached, with `reason` saying why.
fn unavailable(reason: impl Into<String>) -> ServiceProcessError {
    ServiceUnavailable {
        reason: reason.into(),
    }
    .into()
}

/// Environment variable that sets the executable to start the service with,
/// overriding the one in the global config.
#[lore_macro::test_pub]
const SERVICE_EXECUTABLE_VAR: &str = "LORE_SERVICE_EXECUTABLE";

/// Where the global config names an executable, for messages that ask a reader
/// to set it.
#[lore_macro::test_pub]
const SERVICE_EXECUTABLE_SETTING: &str = "[service] executable";

/// Arguments that make the executable run as the service.
const SERVICE_RUN_ARGUMENTS: [&str; 2] = ["service", "run"];

/// How long a caller waits for a service to answer after starting one. It
/// covers starting a process and binding the socket, and when several callers
/// start one at once it also covers the wait for whichever process won the
/// claim, so it is measured in seconds rather than milliseconds. A service that
/// lost the claim waits as long for the winner to answer.
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of that wait is left once the service this caller started has
/// exited. A service that lost the claim exits once a service answers or
/// [`SERVICE_START_TIMEOUT`] passes, and one that cannot run here exits at once;
/// neither is worth the rest of the wait.
const SERVICE_EXITED_GRACE: Duration = Duration::from_secs(2);

/// Delay between connection attempts while waiting for a service to answer.
/// One attempt is one connect on a local socket, so attempting this often costs
/// little and keeps the wait close to how long the service took to bind.
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);

/// How long a stop waits for the socket to be free once the service has
/// acknowledged it.
///
/// Comfortably above the service's own bound on shutting down: it drains the
/// reply, then unwinds its accept loop under a five-second cap of its own. A
/// wait shorter than that would report a failure for a service that was still
/// stopping normally.
const STOP_RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// The executable from the environment variable, trimmed, with a blank value read as unset so
/// that clearing the field removes it.
fn executable_from_environment(from_env: Option<OsString>) -> Option<OsString> {
    from_env.filter(|value| !value.to_string_lossy().trim().is_empty())
}

/// The executable from the config, trimmed, with a blank value read as unset so
/// that clearing the field removes it.
fn executable_from_config(from_config: Option<&str>) -> Option<&str> {
    from_config.map(str::trim).filter(|value| !value.is_empty())
}

/// The executable to start the service with, from the environment variable or
/// the global config.
///
/// This ensures that clients of different versions sharing a machine have a predictable service
/// version.
#[lore_macro::test_pub]
fn resolve_service_executable(
    from_env: Option<OsString>,
    from_config: Option<&str>,
) -> Result<PathBuf, ServiceProcessError> {
    if let Some(executable) = executable_from_environment(from_env) {
        return Ok(PathBuf::from(executable));
    }

    if let Some(executable) = executable_from_config(from_config) {
        return Ok(PathBuf::from(executable));
    }

    Err(unavailable(format!(
        "no service executable set; set one under \
         {SERVICE_EXECUTABLE_SETTING} in the global config, or in {SERVICE_EXECUTABLE_VAR}"
    )))
}

/// The executable to start the service with, reading the one set in the config.
///
/// A config that cannot be read leaves the executable unknown rather than failing
/// the call. Most callers have none set, so a config Lore cannot read is reported
/// and stepped over.
pub async fn service_executable() -> Result<PathBuf, ServiceProcessError> {
    let from_config = match GlobalConfig::load().await {
        Ok(config) => config.service_executable().map(str::to_string),
        Err(error) => {
            lore_warn!(
                "Could not read {SERVICE_EXECUTABLE_SETTING} from the global config: {error}"
            );
            None
        }
    };

    resolve_service_executable(
        std::env::var_os(SERVICE_EXECUTABLE_VAR),
        from_config.as_deref(),
    )
}

/// Environment variable that turns relaying on or off, overriding the setting
/// in the global config.
const USE_SERVICE_VAR: &str = "LORE_USE_SERVICE";

/// Where the global config turns relaying on, for messages naming it.
const USE_SERVICE_SETTING: &str = "[service] use_automatically";

/// The decision already made in this process, so that a config file is read
/// once rather than on every call. A caller that changes the setting from inside
/// the process clears it through [`forget_whether_service_is_in_use`].
static SERVICE_IN_USE: RwLock<Option<bool>> = RwLock::new(None);

/// Whether a value written as a word means off.
///
/// `LORE_USE_SERVICE=0` has to turn relaying off. Reading any non-empty value as
/// on, which is what a bare emptiness check does, makes the obvious way to
/// disable something enable it instead.
fn reads_as_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Whether relaying is asked for, from the environment variable for this command
/// and the setting in the config.
///
/// A blank value reads as unset and defers to the config, matching how a blank
/// `[service] executable` defers to the config.
#[lore_macro::test_pub]
fn relaying_is_asked_for(from_env: Option<OsString>, from_config: bool) -> bool {
    if let Some(from_env) = from_env {
        let from_env = from_env.to_string_lossy();
        if !from_env.trim().is_empty() {
            return !reads_as_off(&from_env);
        }
    }
    from_config
}

/// Reports settings that may not relay as expected, for a caller that has just
/// written one of them.
///
/// Relaying can use a service that is already running, but starting one when
/// none is running requires an executable. A user turning relaying on without
/// setting an executable may not realize commands will fail if no service is
/// running.
pub(crate) fn report_settings_update_that_will_not_relay(config: &GlobalConfig) {
    if !config.use_service_automatically() {
        return;
    }

    let exec_from_env = std::env::var_os(SERVICE_EXECUTABLE_VAR);
    let exec_from_config = config.service_executable();
    if resolve_service_executable(exec_from_env, exec_from_config).is_ok() {
        return;
    }

    lore_warn!(
        "No service executable is set. Commands will use a service that is \
         already running, but will fail if none is running. To let commands \
         start a service when needed, set an executable with \
         `lore service set-executable <path>`, or under \
         {SERVICE_EXECUTABLE_SETTING} in the global config."
    );
}

/// The config values the relaying decision reads, so the two loaders below agree
/// on what they take from a config they could not read.
struct ServiceSettings {
    relaying_on: bool,
}

impl ServiceSettings {
    fn from(config: &GlobalConfig) -> Self {
        Self {
            relaying_on: config.use_service_automatically(),
        }
    }

    /// What an unreadable config yields: relaying off, which is the setting's own
    /// default, so a config Lore cannot read is reported and stepped over rather
    /// than failing the call.
    fn unreadable(error: &impl std::fmt::Display) -> Self {
        lore_warn!("Could not read {USE_SERVICE_SETTING} from the global config: {error}");
        Self { relaying_on: false }
    }

    /// Whether calls are relayed to the service, which is when relaying is asked for.
    ///
    /// An executable is not required here: a service already running can be used
    /// without one. Only starting a service when none is running requires an
    /// executable, and that check happens in [`connect_or_spawn_service`].
    fn decide_use_service(&self) -> bool {
        relaying_is_asked_for(std::env::var_os(USE_SERVICE_VAR), self.relaying_on)
    }
}

/// Whether this call is carried out by the service rather than here.
///
/// Decided once per process: an embedder makes many calls, and each would
/// otherwise read the config file again to learn something that does not change
/// underneath it.
pub(crate) async fn service_in_use() -> bool {
    if let Some(decided) = *SERVICE_IN_USE.read() {
        return decided;
    }

    let settings = match GlobalConfig::load().await {
        Ok(config) => ServiceSettings::from(&config),
        Err(error) => ServiceSettings::unreadable(&error),
    };

    let decided = settings.decide_use_service();
    *SERVICE_IN_USE.write() = Some(decided);
    decided
}

/// Whether calls are relayed, answered without a runtime.
///
/// The runtime is sized before the first call runs, and a relaying process wants
/// a smaller one than a working process, so that decision has to be made before
/// there is a runtime to make it on. Reads the config with the blocking loader
/// for the same reason, and fills the same answer [`service_in_use`] reads, so
/// the file is read once either way.
pub(crate) fn service_in_use_blocking() -> bool {
    if let Some(decided) = *SERVICE_IN_USE.read() {
        return decided;
    }

    let settings = match GlobalConfig::load_blocking() {
        Ok(config) => ServiceSettings::from(&config),
        Err(error) => ServiceSettings::unreadable(&error),
    };

    let decided = settings.decide_use_service();
    *SERVICE_IN_USE.write() = Some(decided);
    decided
}

/// Forgets the decision, so that a setting changed in this process is read again
/// rather than answered from before the change.
pub(crate) fn forget_whether_service_is_in_use() {
    *SERVICE_IN_USE.write() = None;
}

/// Releases the service from the session and terminal of the command starting
/// it, so that it outlives them — and, on Windows, from its caller's standard
/// handles, so that they outlive the service rather than the other way around.
///
/// A service that shares its caller's session is sent the same `SIGHUP` when
/// that terminal closes, which would end it along with the shell that happened
/// to run the first command. On Unix that means its own session, via `setsid` in
/// the child between fork and exec. On Windows it means no inherited console and
/// no console of its own, which would otherwise appear as a stray window.
#[lore_macro::test_pub]
fn detach_from_caller(command: &mut Command) {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::process::CommandExt;

        // Safety: runs in the forked child before exec, where only
        // async-signal-safe calls are allowed. `setsid` is one of them (POSIX
        // lists it as such) and reads no memory through pointers. It fails only
        // for a process that already leads its group, which a fresh child never
        // does, so an error here means something is wrong enough to report
        // rather than to start an attached service over.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        /// `DETACHED_PROCESS`: the service inherits no console from its caller.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        /// `CREATE_NO_WINDOW`: nor does it get a console window of its own.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        keep_standard_handles_out_of_the_service();
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
}

/// Stops this process's standard handles being inherited by the service, so
/// that a caller reading this process's output sees it end.
///
/// `CreateProcessW` with handle inheritance on — which `std::process` uses, to
/// pass the service its `NUL` handles — copies *every* inheritable handle into
/// the child, not only the three it was given. A parent that runs this program
/// with its output piped hands it the pipes' write ends marked inheritable, and
/// an inherited handle keeps that mark, so the service inherits them too. The
/// service is built to outlive this process, so the pipes never close, and the
/// parent waiting to read them to the end waits forever. That is any program
/// capturing `lore`'s output on a machine with the service turned on: a build
/// script, a hook, an editor — measured, it is also this suite's own Windows
/// run, where one such wait ate the whole job.
///
/// Clearing `HANDLE_FLAG_INHERIT` on this process's three standard handles is
/// enough: writes to them are unaffected, and both `std::process` and CPython
/// duplicate a handle afresh — inheritable — when asked to pass one on, so
/// later children that should share these handles still do. The flags are not
/// restored afterwards, because restoring makes a race of it: another thread
/// spawning during the window would leak the handles the same way, sometimes.
///
/// What this does not do is stop the service inheriting *other* inheritable
/// handles a program that links the library may hold. Only a spawn that lists
/// the handles the child may take — `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` — closes
/// that, and from `std::process` that is `Command::inherit_handles` /
/// `raw_attribute`, both unstable at the time of writing. This buys the part
/// that hangs callers, without hand-rolling `CreateProcessW`.
#[cfg(target_os = "windows")]
fn keep_standard_handles_out_of_the_service() {
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Foundation::SetHandleInformation;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
    use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;

    for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // Safety: necessary to call windows APIs. `GetStdHandle` takes no
        // pointers; `SetHandleInformation` is called only on a handle it
        // returned, after screening the two values that mean there is none.
        unsafe {
            let handle = GetStdHandle(standard_handle);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            // Best effort: a flag this cannot clear leaves that handle as
            // inheritable as it was before this function existed.
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// Services started by this process, kept only so that they can be collected.
///
/// Dropping a `Child` leaves the process it names a zombie once it exits, for as
/// long as the process that started it runs. For a command that is no time at
/// all — it exits, and the service is left to init. For a program that links the
/// library and runs for days, a service that started and later stopped would sit
/// in the process table until the program itself ended.
static STARTED_SERVICES: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// Collects the services started here that have since exited.
///
/// Called at the three points a program that outlives its services is known or
/// last able to have lost one: before starting another, which is why it is back
/// here at all; once a stop finds nothing listening, for a program that stops
/// its service and never starts another; and from `lore::shutdown()`, after
/// which no service call will come to do either.
///
/// Not on a timer. What is left uncovered is a service that dies of its own
/// accord in a program that then makes no further service call and never shuts
/// the library down — one process table entry, held until that program exits,
/// against a task polling for the life of every program that ever started a
/// service.
#[lore_macro::test_pub]
pub(crate) fn collect_exited_services() {
    STARTED_SERVICES
        .lock()
        .retain_mut(|service| !matches!(service.try_wait(), Ok(Some(_))));
}

/// Keeps a started service, whether it won the socket or lost it, so that it can
/// be collected once it exits.
#[lore_macro::test_pub]
fn remember_started_service(service: Child) {
    STARTED_SERVICES.lock().push(service);
}

/// Starts a service process running `executable`.
///
/// The service outlives the command that starts it, so it takes none of that
/// command's standard streams: a pipe left open would hold a reader waiting for
/// the service to exit, and anything the service wrote would land in the
/// caller's output. It is released from the caller's session for the same
/// reason — see [`detach_from_caller`].
fn spawn_service(executable: PathBuf) -> Result<Child, ServiceProcessError> {
    lore_debug!("Starting Lore service with {}", executable.display());

    let mut command = Command::new(&executable);
    command
        .args(SERVICE_RUN_ARGUMENTS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_from_caller(&mut command);

    let service = command.spawn().map_err(|error| {
        unavailable(format!("starting {} failed: {error}", executable.display()))
    })?;
    Ok(service)
}

/// One connection attempt.
///
/// Nothing listening is the expected answer while a service is starting, and one
/// wait makes hundreds of attempts, so a refused connection is reported as
/// `None` rather than logged. Only a failure to make the attempt at all is an
/// error.
async fn connect_attempt() -> Result<Option<UdsStream>, ServiceProcessError> {
    let connection =
        lore_base::lore_spawn_blocking!(|| UdsStream::connect(service_socket_name()).ok())
            .await
            .internal("joining the service connect task")?;
    Ok(connection)
}

/// Connects to a service that is already running, without starting one.
///
/// Reports `None` when none is listening, so that a caller which acts only on a
/// running service can tell that apart from a failure to look for one.
pub(crate) async fn connect_to_running_service() -> Result<Option<UdsStream>, ServiceProcessError> {
    if !uds_supported() {
        return Err(unavailable("OS doesn't support IPC"));
    }
    connect_attempt().await
}

/// Connects to the service, starting one when none is running.
///
/// Several callers reach this at once with no service running, and each starts
/// one. Only the first to claim the socket keeps running, and the rest exit once
/// it answers, so what is waited for afterwards is *a* service answering rather
/// than the one this caller started: the callers whose service lost the claim
/// become clients of the one that won it.
pub(crate) async fn connect_or_spawn_service() -> Result<UdsStream, ServiceProcessError> {
    if let Some(connection) = connect_to_running_service().await? {
        return Ok(connection);
    }

    collect_exited_services();

    let executable = service_executable().await?;
    let mut service = lore_base::lore_spawn_blocking!(move || spawn_service(executable))
        .await
        .internal("joining the service start task")??;

    let started = Instant::now();
    let deadline = started + SERVICE_START_TIMEOUT;
    let mut deadline_once_exited = None;
    loop {
        tokio::time::sleep(CONNECT_RETRY_DELAY).await;

        if let Some(connection) = connect_attempt().await? {
            // Kept rather than dropped, whether this is the service that
            // answered or one that lost the socket and exited: either way it is
            // this process's child until it is collected.
            remember_started_service(service);
            return Ok(connection);
        }

        if deadline_once_exited.is_none() && matches!(service.try_wait(), Ok(Some(_))) {
            deadline_once_exited = Some(Instant::now() + SERVICE_EXITED_GRACE);
        }

        let now = Instant::now();
        if now >= deadline || deadline_once_exited.is_some_and(|exited| now >= exited) {
            let elapsed = started.elapsed().as_secs_f32();
            let state = started_service_state(&mut service);
            remember_started_service(service);
            return Err(unavailable(format!(
                "none accepted a connection in {elapsed:.1} seconds; {state}"
            )));
        }
    }
}

/// Waits up to [`SERVICE_START_TIMEOUT`] for a service to accept a connection,
/// and reports whether one did.
pub async fn wait_until_a_service_is_listening() -> Result<bool, ServiceProcessError> {
    wait_for_listening(true, SERVICE_START_TIMEOUT).await
}

/// Waits until nothing is listening.
///
/// A service acknowledges a stop over IPC and then keeps its socket while the
/// reply drains and its accept loop unwinds, so a caller that returns on the
/// acknowledgement leaves the socket held. A script or a test that stops one
/// service and starts another would then race the first one's shutdown, and the
/// second would find the socket taken.
pub(crate) async fn wait_until_no_service_is_listening() -> Result<(), ServiceProcessError> {
    let started = Instant::now();
    if wait_for_listening(false, STOP_RELEASE_TIMEOUT).await? {
        return Ok(());
    }
    // Not `ServiceUnavailable`: one is available, which is the problem.
    Err(ServiceProcessError::internal(format!(
        "a Lore service was asked to stop and was still listening {:.1} seconds later",
        started.elapsed().as_secs_f32()
    )))
}

/// Polls the socket until a service answers on it when `listening`, or until none does
/// otherwise, for up to `timeout`; answers whether that happened in time.
async fn wait_for_listening(
    listening: bool,
    timeout: Duration,
) -> Result<bool, ServiceProcessError> {
    let deadline = Instant::now() + timeout;
    loop {
        if connect_attempt().await?.is_some() == listening {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(CONNECT_RETRY_DELAY).await;
    }
}

/// What became of the service this caller started, to report alongside a wait
/// that ran out. It separates a service that failed to start from one that is
/// running but not answering.
fn started_service_state(service: &mut Child) -> String {
    match service.try_wait() {
        Ok(Some(status)) => format!("the service started here exited with {status}"),
        Ok(None) => "the service started here is still running".to_string(),
        Err(error) => format!("the service started here cannot be waited on: {error}"),
    }
}

/// The stop request for a service running in this process.
#[lore_macro::test_pub]
struct StopRequest {
    /// Set once a stop has been asked for. Read on its own so that a wait
    /// cannot miss a request which lands before the wait parks.
    requested: AtomicBool,
    notify: Notify,
}

/// Held by the process that runs the service. It is both the request that
/// process waits on and the record that a service runs here at all.
static STOP_REQUEST: OnceLock<Arc<StopRequest>> = OnceLock::new();

/// The stop request the process running the service waits on.
#[lore_macro::test_pub]
pub struct ServiceStopRequest {
    request: Arc<StopRequest>,
}

impl ServiceStopRequest {
    /// Resolves once a stop has been requested, including one requested before
    /// the wait began.
    pub async fn requested(&self) {
        while !self.request.requested.load(Ordering::Acquire) {
            self.request.notify.notified().await;
        }
    }
}

/// Records that this process runs the service and returns the stop request it
/// waits on. Repeated calls return the same request.
pub fn register_service_process() -> ServiceStopRequest {
    let request = STOP_REQUEST.get_or_init(|| {
        Arc::new(StopRequest {
            requested: AtomicBool::new(false),
            notify: Notify::new(),
        })
    });
    ServiceStopRequest {
        request: Arc::clone(request),
    }
}

/// Whether the service runs in this process, which is what makes a stop act on
/// it rather than reach for a socket this process owns.
#[lore_macro::test_pub]
pub(crate) fn service_runs_in_this_process() -> bool {
    STOP_REQUEST.get().is_some()
}

/// Asks the service running in this process to stop, and reports whether there
/// was one to ask. The request is stored before the wait is woken, so a wait
/// that reads it afterwards sees the request either way.
#[lore_macro::test_pub]
pub(crate) fn request_service_stop() -> bool {
    let Some(request) = STOP_REQUEST.get() else {
        return false;
    };
    request.requested.store(true, Ordering::Release);
    request.notify.notify_one();
    true
}
