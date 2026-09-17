// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service process a call is served by: connecting to the one that runs,
//! starting one when none does, and stopping the one that runs in this process.
use std::ffi::OsString;
use std::path::Path;
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

/// Name of the executable that carries the service, without the platform's
/// executable suffix.
const SERVICE_EXECUTABLE_NAME: &str = "lore";

/// Names the executable to start the service with for one call, overriding the
/// one named in the global config.
const SERVICE_EXECUTABLE_VAR: &str = "LORE_SERVICE_EXECUTABLE";

/// Where the global config names an executable, for messages that ask a reader
/// to set it.
const SERVICE_EXECUTABLE_SETTING: &str = "[service] executable";

/// Arguments that make the executable run as the service.
const SERVICE_RUN_ARGUMENTS: [&str; 2] = ["service", "run"];

/// How long a caller waits for a service to answer after starting one. It
/// covers starting a process and binding the socket, and when several callers
/// start one at once it also covers the wait for whichever process won the
/// socket, so it is measured in seconds rather than milliseconds.
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of that wait is left once the service this caller started has
/// exited. A service exits either because another one holds the socket, which
/// means one is listening already, or because none can run here at all, and
/// neither outcome is worth the rest of the wait.
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

/// File name of the executable that carries the service on this platform.
fn service_executable_name() -> String {
    format!("{SERVICE_EXECUTABLE_NAME}{}", std::env::consts::EXE_SUFFIX)
}

/// Whether `path` names the executable that carries the service.
///
/// The whole file name is compared rather than the stem. A stem comparison also
/// accepts `lore.plugin` and `lore.sh`, so an embedding program with a name like
/// that would be relaunched as the service instead of the client beside it being
/// found — starting the wrong program, and leaving no service able to start.
///
/// Case is ignored only where the platform's own paths ignore it. On Unix `LORE`
/// names a different file from `lore`, so matching it there would resolve a path
/// to a program that is not the client, or to nothing at all.
fn is_service_executable(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.as_encoded_bytes();
        let expected = service_executable_name();
        if cfg!(target_family = "windows") {
            name.eq_ignore_ascii_case(expected.as_bytes())
        } else {
            name == expected.as_bytes()
        }
    })
}

/// The executable named for one call, with a value of nothing but space read as
/// unset.
///
/// Left to stand, such a value would be started as a path made of spaces and
/// fail with an error about a file nobody named. A set value is otherwise passed
/// through as it came: a path is not required to be text this can trim.
fn named_executable(named: Option<OsString>) -> Option<OsString> {
    named.filter(|value| !value.to_string_lossy().trim().is_empty())
}

/// The executable the config pins, trimmed, with a blank value read as unset so
/// that clearing the field stops pinning one.
fn pinned_executable(pinned: Option<&str>) -> Option<&str> {
    pinned.map(str::trim).filter(|value| !value.is_empty())
}

/// The executable to start the service with, from the one named for this call,
/// the one the global config pins, and the running program's own path.
///
/// The order is what makes the choice deliberate. Clients of different versions
/// share a machine, and without a pin the version that serves them is decided by
/// whichever client happened to start a service first. The config names that
/// version once; the environment overrides it for a single call, for a build
/// under test or a one-off. Only with neither set does this fall back to the
/// running program, which is the Lore client itself in the ordinary case. A
/// caller that links the library runs its own program, which must not be started
/// a second time, so the client shipped beside it is used instead.
///
/// That fallback serves a command that asks for a service outright, which is a
/// deliberate act by whoever ran it. Relaying automatically requires a name — see
/// [`service_executable_is_named`].
fn resolve_service_executable(
    named: Option<OsString>,
    pinned: Option<&str>,
    running: std::io::Result<PathBuf>,
) -> Result<PathBuf, ServiceProcessError> {
    if let Some(named) = named_executable(named) {
        return Ok(PathBuf::from(named));
    }

    if let Some(pinned) = pinned_executable(pinned) {
        return Ok(PathBuf::from(pinned));
    }

    let running = running.internal("reading the running program's path")?;
    if is_service_executable(&running) {
        return Ok(running);
    }

    let beside = running.with_file_name(service_executable_name());
    if beside.is_file() {
        return Ok(beside);
    }

    Err(unavailable(format!(
        "no {} to start one with beside {}; name one under \
         {SERVICE_EXECUTABLE_SETTING} in the global config, or in {SERVICE_EXECUTABLE_VAR}",
        service_executable_name(),
        running.display()
    )))
}

/// The executable to start the service with, reading the config that pins one.
///
/// A config that cannot be read leaves the pin unknown rather than failing the
/// call. Most callers have none set, and the remaining rules still resolve an
/// executable, so a config Lore cannot read is reported and stepped over.
pub async fn service_executable() -> Result<PathBuf, ServiceProcessError> {
    let pinned = match GlobalConfig::load().await {
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
        pinned.as_deref(),
        std::env::current_exe(),
    )
}

/// Turns relaying on or off for one command, overriding the setting in the
/// global config.
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

/// Whether relaying is asked for, from the value named for this command and the
/// setting in the config.
///
/// A blank value reads as unset and defers to the config, matching how a blank
/// `[service] executable` defers to the rules below it.
fn relaying_is_asked_for(named: Option<OsString>, configured: bool) -> bool {
    if let Some(named) = named {
        let named = named.to_string_lossy();
        if !named.trim().is_empty() {
            return !reads_as_off(&named);
        }
    }
    configured
}

/// Whether an executable is named for the service, in either of the two places
/// that can name one.
///
/// This is required before calls relay automatically, and turning relaying on is
/// not enough on its own. With no name the executable resolves from the running
/// program, so the version serving a machine would be whichever program relayed
/// first — an editor's bundled plugin as readily as the client someone installed,
/// and thereafter every client on the machine is served by that one. Naming it
/// makes the version serving a machine something chosen and readable rather than
/// an accident of ordering.
///
/// A command that asks for a service outright still needs no name: `lore service
/// start` is a deliberate act by whoever ran it, and resolves through the whole
/// chain in [`resolve_service_executable`].
fn service_executable_is_named(named: Option<OsString>, pinned: Option<&str>) -> bool {
    named_executable(named).is_some() || pinned_executable(pinned).is_some()
}

/// Whether calls are relayed to the service, which requires both that relaying is
/// asked for and that an executable is named for it.
///
/// Relaying asked for with no executable named is reported rather than passed
/// over, since the setting has been turned on and is not taking effect, and
/// nothing else about the command would say so.
fn use_service(named: Option<OsString>, configured: bool, executable_is_named: bool) -> bool {
    if !relaying_is_asked_for(named, configured) {
        return false;
    }

    if !executable_is_named {
        // Reaches a log file rather than a terminal: this decision is made before
        // the call has an execution context to dispatch a message through. The
        // report a reader can act on comes from the setters instead — see
        // [`report_settings_that_will_not_relay`].
        lore_warn!(
            "{USE_SERVICE_SETTING} is on with no service executable named, so this \
             call runs here rather than on the service"
        );
        return false;
    }

    true
}

/// Reports a pair of settings that will not relay, for a caller that has just
/// written one of them.
///
/// Relaying needs both, and either one alone says nothing about the other, so
/// turning the setting on and finding nothing changed would otherwise be silent.
///
/// Judged on the config alone. `LORE_SERVICE_EXECUTABLE` names an executable for
/// one command, which is not the same as the machine having one configured.
pub(crate) fn report_settings_that_will_not_relay(config: &GlobalConfig) {
    if config.use_service_automatically() && config.service_executable().is_none() {
        lore_warn!(
            "No service executable is named, so commands will keep running in the \
             process that runs them. Name the build to serve this machine with \
             `lore service set-executable <path>`, or under \
             {SERVICE_EXECUTABLE_SETTING} in the global config."
        );
    }
}

/// The config values the relaying decision reads, so the two loaders below agree
/// on what they take from a config they could not read.
struct ServiceSettings {
    relaying_on: bool,
    executable: Option<String>,
}

impl ServiceSettings {
    fn from(config: &GlobalConfig) -> Self {
        Self {
            relaying_on: config.use_service_automatically(),
            executable: config.service_executable().map(str::to_string),
        }
    }

    /// What an unreadable config yields: relaying off, which is the setting's own
    /// default, so a config Lore cannot read is reported and stepped over rather
    /// than failing the call.
    fn unreadable(error: &impl std::fmt::Display) -> Self {
        lore_warn!("Could not read {USE_SERVICE_SETTING} from the global config: {error}");
        Self {
            relaying_on: false,
            executable: None,
        }
    }

    fn decide(&self) -> bool {
        use_service(
            std::env::var_os(USE_SERVICE_VAR),
            self.relaying_on,
            service_executable_is_named(
                std::env::var_os(SERVICE_EXECUTABLE_VAR),
                self.executable.as_deref(),
            ),
        )
    }
}

/// Whether a unit test of this crate is what is running, which never relays.
///
/// A test sets its own fixture up in the process running it, and a service knows
/// nothing of that fixture, so relaying a test's calls fails them wholesale — 202
/// of this crate's unit tests, measured, on a machine with the service turned on.
/// A developer who turns it on for their own use must still be able to run the
/// tests.
///
/// Only the unit tests: an integration test links this crate compiled without
/// `cfg(test)`, so this is false there. Those are covered by `LORE_USE_SERVICE=0`
/// in `.cargo/config.toml`, which covers the unit tests too when they are run
/// through cargo. This is here for when they are not — an editor's test runner,
/// or a test binary run directly.
fn a_unit_test_is_running() -> bool {
    cfg!(test)
}

/// Whether this call is carried out by the service rather than here.
///
/// Decided once per process: an embedder makes many calls, and each would
/// otherwise read the config file again to learn something that does not change
/// underneath it.
pub(crate) async fn service_in_use() -> bool {
    if a_unit_test_is_running() {
        return false;
    }

    if let Some(decided) = *SERVICE_IN_USE.read() {
        return decided;
    }

    let settings = match GlobalConfig::load().await {
        Ok(config) => ServiceSettings::from(&config),
        Err(error) => ServiceSettings::unreadable(&error),
    };

    let decided = settings.decide();
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
    if a_unit_test_is_running() {
        return false;
    }

    if let Some(decided) = *SERVICE_IN_USE.read() {
        return decided;
    }

    let settings = match GlobalConfig::load_blocking() {
        Ok(config) => ServiceSettings::from(&config),
        Err(error) => ServiceSettings::unreadable(&error),
    };

    let decided = settings.decide();
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
pub(crate) fn collect_exited_services() {
    STARTED_SERVICES
        .lock()
        .retain_mut(|service| !matches!(service.try_wait(), Ok(Some(_))));
}

/// Keeps a started service, whether it won the socket or lost it, so that it can
/// be collected once it exits.
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
    let connection = lore_base::lore_spawn_blocking!(|| UdsStream::connect().ok())
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
/// one. Only the first to bind the socket keeps running and the rest exit, so
/// what is waited for afterwards is *a* service answering rather than the one
/// this caller started: the callers whose service lost the socket become
/// clients of the one that won it.
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

/// Waits until nothing is listening.
///
/// A service acknowledges a stop over IPC and then keeps its socket while the
/// reply drains and its accept loop unwinds, so a caller that returns on the
/// acknowledgement leaves the socket held. A script or a test that stops one
/// service and starts another would then race the first one's shutdown, and the
/// second would find the socket taken.
pub(crate) async fn wait_until_no_service_is_listening() -> Result<(), ServiceProcessError> {
    let started = Instant::now();
    let deadline = started + STOP_RELEASE_TIMEOUT;

    loop {
        if connect_attempt().await?.is_none() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            // Not `ServiceUnavailable`: one is available, which is the problem.
            return Err(ServiceProcessError::internal(format!(
                "a Lore service was asked to stop and was still listening {:.1} seconds later",
                started.elapsed().as_secs_f32()
            )));
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
pub(crate) fn service_runs_in_this_process() -> bool {
    STOP_REQUEST.get().is_some()
}

/// Asks the service running in this process to stop, and reports whether there
/// was one to ask. The request is stored before the wait is woken, so a wait
/// that reads it afterwards sees the request either way.
pub(crate) fn request_service_stop() -> bool {
    let Some(request) = STOP_REQUEST.get() else {
        return false;
    };
    request.requested.store(true, Ordering::Release);
    request.notify.notify_one();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test module's own, named after the test using it.
    fn test_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("lore-service-process-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the test directory must be creatable");
        path
    }

    #[test]
    fn the_client_starts_the_service_with_itself() {
        let client = Path::new("/opt/lore/bin").join(service_executable_name());

        let resolved = resolve_service_executable(None, None, Ok(client.clone()))
            .expect("the client resolves an executable");

        assert_eq!(resolved, client);
    }

    #[test]
    fn a_program_that_links_the_library_starts_the_client_beside_it() {
        let directory = test_directory("client-beside-the-program");
        let client = directory.join(service_executable_name());
        std::fs::write(&client, b"").expect("the client stand-in must be writable");

        let resolved = resolve_service_executable(None, None, Ok(directory.join("editor")))
            .expect("a program with a client beside it resolves that client");

        assert_eq!(resolved, client);
    }

    #[test]
    fn a_program_with_no_client_beside_it_is_told_to_name_one() {
        let directory = test_directory("no-client-beside-the-program");

        let error = resolve_service_executable(None, None, Ok(directory.join("editor")))
            .expect_err("a program with no client beside it cannot start the service");

        assert!(
            error.to_string().contains(SERVICE_EXECUTABLE_SETTING)
                && error.to_string().contains(SERVICE_EXECUTABLE_VAR),
            "the failure must name both places one can be set: {error}"
        );
    }

    #[test]
    fn the_pinned_executable_is_used_ahead_of_the_running_client() {
        // The running program is a client and could serve, but the config names
        // the version that does, so a machine with several installed agrees on
        // one rather than on whichever started first.
        let pinned = "/opt/lore/1.9/bin/lore";
        let running = Path::new("/opt/lore/1.2/bin").join(service_executable_name());

        let resolved = resolve_service_executable(None, Some(pinned), Ok(running))
            .expect("the pinned executable resolves");

        assert_eq!(resolved, PathBuf::from(pinned));
    }

    /// A pin of nothing but space would otherwise be started as a path made of
    /// spaces, which fails with an error about a file no one named.
    #[test]
    fn a_pin_of_only_space_falls_back_to_the_running_client() {
        let client = Path::new("/opt/lore/bin").join(service_executable_name());

        let resolved = resolve_service_executable(None, Some("   "), Ok(client.clone()))
            .expect("a blank pin resolves the running client");

        assert_eq!(resolved, client);
    }

    #[test]
    fn a_named_value_that_reads_as_off_turns_relaying_off() {
        // The obvious way to disable something must not enable it.
        for off in ["0", "false", "no", "off", "OFF", "False", " 0 "] {
            assert!(
                !relaying_is_asked_for(Some(OsString::from(off)), true),
                "{off} must turn relaying off even where the config turns it on"
            );
        }
    }

    #[test]
    fn a_named_value_that_reads_as_on_turns_relaying_on() {
        for on in ["1", "true", "yes", "on", "anything"] {
            assert!(
                relaying_is_asked_for(Some(OsString::from(on)), false),
                "{on} must turn relaying on even where the config leaves it off"
            );
        }
    }

    #[test]
    fn no_named_value_leaves_the_config_to_decide() {
        assert!(
            relaying_is_asked_for(None, true),
            "the config turns relaying on"
        );
        assert!(!relaying_is_asked_for(None, false), "and off");
        // Blank reads as unset, as a blank pin does.
        assert!(relaying_is_asked_for(Some(OsString::from("   ")), true));
        assert!(!relaying_is_asked_for(Some(OsString::new()), false));
    }

    /// Turning relaying on is not enough on its own. Without a named executable
    /// the version serving a machine would be whichever program relayed first, so
    /// an editor's bundled plugin could take a machine's service for itself.
    #[test]
    fn relaying_turned_on_without_a_named_executable_stays_off() {
        assert!(
            !use_service(None, true, false),
            "the config turning relaying on must not relay with no executable named"
        );
        assert!(
            !use_service(Some(OsString::from("1")), false, false),
            "and nor must the environment turning it on"
        );
    }

    #[test]
    fn relaying_turned_on_with_a_named_executable_relays() {
        assert!(use_service(None, true, true));
        assert!(use_service(Some(OsString::from("1")), false, true));
    }

    /// The executable alone asks for nothing. Naming one says which build would
    /// serve the machine, not that anything should be relayed to it.
    #[test]
    fn a_named_executable_alone_does_not_turn_relaying_on() {
        assert!(!use_service(None, false, true));
        assert!(
            !use_service(Some(OsString::from("off")), true, true),
            "a named value that reads as off still wins over both"
        );
    }

    #[test]
    fn an_executable_named_in_either_place_counts_as_named() {
        assert!(service_executable_is_named(
            Some(OsString::from("/opt/lore/bin/lore")),
            None
        ));
        assert!(service_executable_is_named(
            None,
            Some("/opt/lore/bin/lore")
        ));
        assert!(!service_executable_is_named(None, None));
    }

    /// Blank counts as unset for this the same way it does when resolving the
    /// executable, or relaying would turn on for a name that resolves to nothing.
    #[test]
    fn a_blank_executable_does_not_count_as_named() {
        assert!(!service_executable_is_named(
            Some(OsString::from("   ")),
            Some("  ")
        ));
        assert!(!service_executable_is_named(
            Some(OsString::new()),
            Some("")
        ));
    }

    #[test]
    fn an_empty_pin_falls_back_to_the_running_client() {
        let client = Path::new("/opt/lore/bin").join(service_executable_name());

        let resolved = resolve_service_executable(None, Some(""), Ok(client.clone()))
            .expect("an empty pin resolves the running client");

        assert_eq!(resolved, client);
    }

    #[test]
    fn the_named_executable_overrides_the_pin() {
        // One call, one build under test, without editing what the machine pins.
        let named = PathBuf::from("/home/dev/lore/target/debug/lore");

        let resolved = resolve_service_executable(
            Some(named.clone().into_os_string()),
            Some("/opt/lore/1.9/bin/lore"),
            Ok(PathBuf::from("/opt/lore/1.2/bin/lore")),
        )
        .expect("the named executable resolves");

        assert_eq!(resolved, named);
    }

    #[test]
    fn the_named_executable_is_used_as_it_stands() {
        let named = PathBuf::from("/opt/tools/lore-for-the-service");

        let resolved = resolve_service_executable(
            Some(named.clone().into_os_string()),
            None,
            Ok(PathBuf::from("/opt/editor/editor")),
        )
        .expect("the named executable resolves");

        assert_eq!(resolved, named);
    }

    #[test]
    fn an_empty_name_falls_back_to_the_running_program() {
        let client = Path::new("/opt/lore/bin").join(service_executable_name());

        let resolved = resolve_service_executable(Some(OsString::new()), None, Ok(client.clone()))
            .expect("an empty name resolves the running client");

        assert_eq!(resolved, client);
    }

    #[test]
    fn a_program_whose_name_only_contains_the_client_name_is_not_the_client() {
        let directory = test_directory("client-name-inside-another-name");

        let error = resolve_service_executable(None, None, Ok(directory.join("lore-server")))
            .expect_err("a name the client's name is only part of is not the client");

        assert!(
            error.to_string().contains(SERVICE_EXECUTABLE_SETTING)
                && error.to_string().contains(SERVICE_EXECUTABLE_VAR),
            "the failure must name both places one can be set: {error}"
        );
    }

    /// A stem comparison accepts every one of these, since each has the stem
    /// `lore`. Accepting one relaunches the embedding program as the service:
    /// the wrong program starts, and no service ever binds the socket.
    #[test]
    fn a_program_that_shares_the_client_name_before_an_extension_is_not_the_client() {
        for name in ["lore.plugin", "lore.sh", "lore.dll", "lore.so"] {
            // Skip the name that is the client on this platform, which `.exe`
            // makes `lore.dll` on none of them but keeps the loop honest.
            if name == service_executable_name() {
                continue;
            }

            let program = test_directory("client-name-before-an-extension").join(name);
            assert!(
                !is_service_executable(&program),
                "{name} must not read as the client"
            );

            resolve_service_executable(None, None, Ok(program))
                .expect_err("a program that is not the client must not be started as the service");
        }
    }

    /// Unix paths are case sensitive, so `LORE` there names a different file —
    /// resolving it would start something that is not the client, or nothing.
    #[cfg(target_family = "unix")]
    #[test]
    fn a_differently_cased_name_is_not_the_client_on_unix() {
        assert!(!is_service_executable(Path::new("/opt/lore/bin/LORE")));
    }

    /// Windows paths ignore case, so `LORE.EXE` there is the same file.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_differently_cased_name_is_the_client_on_windows() {
        assert!(is_service_executable(Path::new("C:\\lore\\bin\\LORE.EXE")));
    }

    #[test]
    fn the_client_reads_as_the_client() {
        assert!(is_service_executable(
            &Path::new("/opt/lore/bin").join(service_executable_name())
        ));
    }

    #[tokio::test]
    async fn a_stop_requested_before_the_wait_still_ends_it() {
        let request = Arc::new(StopRequest {
            requested: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let wait = ServiceStopRequest {
            request: Arc::clone(&request),
        };

        request.requested.store(true, Ordering::Release);
        request.notify.notify_one();

        wait.requested().await;
    }

    #[tokio::test]
    async fn a_stop_requested_during_the_wait_ends_it() {
        let request = Arc::new(StopRequest {
            requested: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let wait = ServiceStopRequest {
            request: Arc::clone(&request),
        };

        // Both run on this task: the wait parks first, then the request lands.
        tokio::join!(wait.requested(), async {
            tokio::task::yield_now().await;
            request.requested.store(true, Ordering::Release);
            request.notify.notify_one();
        });
    }

    /// The session a process belongs to, read from `/proc`. `comm` can hold
    /// spaces and parentheses, so the fields after it are counted from the last
    /// `)`: state, ppid, pgrp, then session.
    #[cfg(target_os = "linux")]
    fn session_of(pid: u32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        stat.rsplit_once(')')
            .map(|(_comm, rest)| rest)
            .unwrap_or_default()
            .split_whitespace()
            .nth(3)
            .and_then(|session| session.parse().ok())
            .unwrap_or(-1)
    }

    /// A service started here is this process's child until it is collected, so
    /// one that has exited must not be left in the process table.
    #[cfg(target_family = "unix")]
    #[test]
    fn a_started_service_that_has_exited_is_collected() {
        let mut started = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("a stand-in process must start");
        let pid = started.id();
        // Wait for it to exit without collecting it, which is the state a
        // dropped handle would leave behind.
        while !matches!(started.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(10));
        }

        // A collected process is no longer waitable, so a second wait on the pid
        // is what tells the two apart.
        remember_started_service(started);
        collect_exited_services();

        // Safety: waits on a pid this test started, writing only to `status`.
        let waited = unsafe {
            let mut status = 0;
            libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG)
        };
        assert_eq!(
            waited, -1,
            "the process must already have been collected, leaving nothing to wait on"
        );
    }

    /// A service has to outlive the terminal that started it, which means
    /// leaving the caller's session rather than being signalled along with it.
    /// Read from the started process rather than from the flags asked for, since
    /// the flags being set is not the property that matters.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_started_service_leaves_the_callers_session() {
        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_from_caller(&mut command);

        let mut started = command.spawn().expect("a stand-in process must start");
        let started_session = session_of(started.id());
        let _ = started.kill();
        let _ = started.wait();

        // Safety: reads no memory through pointers and cannot fail for 0.
        let our_session = unsafe { libc::getsid(0) };

        assert_ne!(started_session, -1, "the session must be readable");
        assert_ne!(
            started_session, our_session,
            "a started service must lead a session of its own"
        );
    }

    #[test]
    fn a_process_running_no_service_has_no_stop_to_request() {
        // No test registers a service process, so nothing recorded a request.
        assert!(!service_runs_in_this_process());
        assert!(!request_service_stop());
    }
}
