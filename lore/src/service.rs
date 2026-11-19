// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(any(target_os = "windows", target_family = "unix"))]
pub mod external_root;
pub mod initialization;
pub mod service_main;
#[cfg(any(target_os = "windows", target_family = "unix"))]
pub mod set_external_directory;
pub mod state;

use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::error::MountsBusy;
use lore_base::error::ServiceUnavailable;
use lore_base::log::LoreLogLevel;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::LoreExternalRootState;
use lore_revision::event::LoreServiceMessageEventData;
use lore_revision::event::LoreServiceStatusEventData;
use lore_revision::fs::mount_manager::MountManager;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::global::GlobalConfig;
use lore_revision::global::external_dir;
use lore_revision::global::external_dir::leftovers::Leftovers;
use lore_revision::global::external_dir::reported_root;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore_info;
use lore_revision::lore_warn;
use lore_revision::relay::EventDispatcher;
use lore_revision::service_state::ServiceStateImpl;
use lore_revision::util::config::SaveableConfig;

use crate::call::no_repository_call;
use crate::call_delegation::refuse_handed_back;
use crate::interface::LoreArray;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreString;
use crate::remote::call::EXTERNAL_ROOT_MOVING;
use crate::remote::call::HandedBack;
use crate::remote::call::Relayed;
use crate::remote::call::run_service_call;
use crate::remote::network::UdsStream;
use crate::remote::service_process::Reached;
use crate::remote::service_process::ServiceProcessError;
use crate::remote::service_process::ServiceStart;
use crate::remote::service_process::collect_exited_services;
use crate::remote::service_process::forget_whether_service_is_in_use;
use crate::remote::service_process::reach_running_service;
use crate::remote::service_process::report_settings_update_that_will_not_relay;
use crate::remote::service_process::request_service_stop;
use crate::remote::service_process::serves_from_this_executable;
use crate::remote::service_process::service_runs_in_this_process;
use crate::remote::service_process::start_and_connect;
use crate::remote::service_process::wait_until_no_service_is_listening;
use crate::service::service_main::service_main;
#[cfg(any(target_os = "windows", target_family = "unix"))]
use crate::service::set_external_directory::Form;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(start_local)]
/// Arguments for starting the Lore service process (no parameters).
pub struct LoreServiceStartArgs {}

/// Start the Lore service process, unless one is already running.
///
/// Connects to the running service, and starts one when nothing is listening: from the
/// executable named in `LORE_SERVICE_EXECUTABLE` or `[service] executable`, else from this
/// program's own where it runs the service ([`crate::serve_from_this_executable`]). Then asks it
/// for its status. Succeeds once a service is reachable, whether it was already running or was
/// started here, and at once in a program whose service has claimed the service socket, while it
/// may still be starting; fails with `ServiceUnavailable` while a move of the external root holds the
/// service socket. The messages the service buffered arrive as `Log` events.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn start(
    globals: LoreGlobalArgs,
    args: LoreServiceStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Starting the service is not work the service does for a caller: it acts on
    // whichever service is reachable from here, so it runs where it was called
    // rather than routing through the service the way the other commands do.
    start_local(globals, args, callback).await
}

fn start_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStartArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, start, move |_args| async move {
        if service_runs_in_this_process() {
            // The command reached the service itself, which is running, and a
            // start asks for nothing more than that.
            return Ok(());
        }

        let connection = match reach_running_service().await? {
            Reached::Here => return Ok(()),
            Reached::Connected(connection) => connection,
            Reached::Nothing => start_and_connect(ServiceStart::NamedOrThisProgram).await?,
        };
        if service_runs_in_this_process() {
            return Ok(());
        }
        relay_a_status(connection).await?;
        lore_info!("Lore service is running");
        Ok::<(), ServiceProcessError>(())
    })
}

/// Relays a status request over `connection`, failing only on the move reply, with each buffered
/// service message re-emitted as a `Log` event and nothing else, from an execution of its own.
async fn relay_a_status(connection: UdsStream) -> Result<(), ServiceProcessError> {
    let start = lore_revision::lore::execution_context();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::ServiceMessage(message) = event {
            start
                .dispatcher
                .send(LoreEvent::Log(EventDispatcher::make_log(
                    message.level,
                    message.message.to_string(),
                )));
        }
    }));
    let relay = run_service_call(
        Some(connection),
        LoreGlobalArgs::default(),
        LoreServiceStatusArgs {}.into(),
        callback,
    );
    let quiet = crate::call::setup_execution(LoreGlobalArgs::default(), None);
    match LORE_CONTEXT.scope(quiet, relay).await {
        Relayed::Ran(_) => Ok(()),
        Relayed::HandedBack(_) => Err(ServiceUnavailable {
            reason: format!("A start reaches no service, and {EXTERNAL_ROOT_MOVING}"),
        }
        .into()),
    }
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(stop_local)]
/// Arguments for stopping the Lore service process.
pub struct LoreServiceStopArgs {}

#[error_set]
pub enum ServiceStopError {
    MountsBusy,
}

/// Stop the running Lore service process.
///
/// Does not start a service to stop, and succeeds when none is running, since
/// that is the state it asks for. A mount the service cannot release because
/// something holds it refuses the stop with `MountsBusy`, unless the global
/// `force` argument is set, which detaches busy mounts lazily. Fails with
/// `ServiceUnavailable` while a move of the external root holds the service
/// socket.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn stop(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Stopping the service acts on whichever service is reachable from here, so
    // like [`start`] it runs where it was called. Routing it through the service
    // branch of the dispatch would also make a stop start one.
    stop_local(globals, args, callback).await
}

/// Stops the service that runs in this process, unmounting its FUSE instances first.
fn stop_here(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, stop, move |_args| async move {
        // A busy mount keeps the service serving it, so the stop is refused before the
        // service is asked to exit. `force` is read from the execution context: read from
        // `globals` before they move, it would keep a second copy of them in the future of
        // `stop_local`.
        // SWFS mounts are closed as the service exits, once it takes no more commands.
        #[cfg(target_os = "linux")]
        if let Some(manager) = MountManagerState::mount_manager() {
            manager
                .fuse()
                .unmount_all(lore_revision::lore::execution_context().globals().force())
                .await
                .forward_any::<ServiceStopError>("Unmounting before the service stops")?;
        }
        request_service_stop();
        Ok::<(), ServiceStopError>(())
    })
}

async fn stop_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> i32 {
    if service_runs_in_this_process() {
        return stop_here(globals, args, callback).await;
    }

    // Reached in the process the caller ran. Hand the command to the service so
    // that the branch above runs there, over a connection made here: a stop acts
    // on a service that is running and must not be what starts one.
    match reach_running_service().await {
        Ok(Reached::Here) => stop_here(globals, args, callback).await,
        Ok(Reached::Connected(connection)) => {
            let status = match run_service_call(Some(connection), globals, args.into(), callback)
                .await
            {
                Relayed::Ran(status) => status,
                Relayed::HandedBack(call) => {
                    return refuse_handed_back(call, "stop", "A stop needs a running Lore service")
                        .await;
                }
            };
            if status != 0 {
                return status;
            }

            // The service acknowledges before it lets go of the socket, so a
            // stop that returned on the acknowledgement alone would leave the
            // next start racing this shutdown.
            match wait_until_no_service_is_listening().await {
                Ok(()) => {
                    // Nothing is listening, so a service this process started has
                    // gone. Collected here as well as before the next start,
                    // because a program that starts one service and stops it
                    // without starting another would otherwise hold the exited
                    // child for as long as it ran.
                    collect_exited_services();
                    0
                }
                Err(error) => {
                    // Reported through the return value alone. The service
                    // already sent the `Complete` for the stop it carried out,
                    // and that stop did succeed; this is the wait after it. See
                    // the note in the commit for what making the two agree
                    // would cost.
                    lore_warn!("{error}");
                    error.ffi_code()
                }
            }
        }
        Ok(Reached::Nothing) => {
            // Nothing was listening, so a service started here has already gone.
            collect_exited_services();
            no_repository_call(globals, callback, args, stop, move |_args| async move {
                // A stop asks for no service to be running, which is already so.
                lore_info!("No Lore service is running");
                Ok::<(), ServiceProcessError>(())
            })
            .await
        }
        Err(error) => {
            no_repository_call(globals, callback, args, stop, move |_args| async move {
                Err::<(), ServiceProcessError>(error)
            })
            .await
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(set_executable_local)]
/// Arguments for naming the executable the Lore service runs from.
pub struct LoreServiceSetExecutableArgs {
    /// Path of the executable to start as the service. Empty clears the setting,
    /// which prevents auto-starting the service but can still can connect to an
    /// already running service.
    pub executable: LoreString,
}

/// Name the executable the Lore service runs from, for this machine.
///
/// Stored in the user-level global config, so it holds for later commands and
/// for clients that read it afterwards. Naming it decides which build serves the
/// machine, rather than leaving that to whichever client starts a service first.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn set_executable(
    globals: LoreGlobalArgs,
    args: LoreServiceSetExecutableArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Like [`start`] and [`stop`], this runs where it was called. A setter that
    // routed would start a service in order to be told which executable to have
    // started.
    set_executable_local(globals, args, callback).await
}

async fn set_executable_local(
    globals: LoreGlobalArgs,
    args: LoreServiceSetExecutableArgs,
    callback: LoreEventCallback,
) -> i32 {
    let dry_run = globals.dry_run();
    let command =
        async move |args: LoreServiceSetExecutableArgs| -> Result<(), ServiceProcessError> {
            let (mut config, lock) = GlobalConfig::load_locked()
                .await
                .forward::<ServiceProcessError>("loading global config")?;

            // Blank clears rather than stores, matching how a blank value already
            // reads as unset everywhere it is resolved.
            let executable = args.executable.as_str().trim();
            config.service.executable = (!executable.is_empty()).then(|| executable.to_string());
            let tense = if dry_run { "would be " } else { "" };
            match config.service_executable() {
                Some(executable) => {
                    lore_info!("Lore service executable {tense}set to {executable}");
                }
                None => lore_info!("Lore service executable {tense}cleared"),
            }
            // Clearing the executable is one of the two ways to end up with a pair
            // that does not relay.
            report_settings_update_that_will_not_relay(&config);
            if dry_run {
                return Ok(());
            }

            config
                .save(lock)
                .await
                .forward::<ServiceProcessError>("saving global config")?;

            // Whether calls relay depends on this as well as on the setting, and
            // this process decided that once. Without forgetting it, an embedder
            // that names an executable goes on relaying nothing, or one that
            // clears it goes on relaying to a build it no longer names.
            forget_whether_service_is_in_use();
            Ok(())
        };
    no_repository_call(globals, callback, args, set_executable, command).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(set_external_directory_local)]
/// Arguments for moving the external root, where every virtual instance's external directory
/// lives.
pub struct LoreServiceSetExternalDirectoryArgs {
    /// The new external root: an absolute path, one relative to the caller's working directory,
    /// or empty for the default location in the global data directory.
    pub path: LoreString,
    /// Point the setting at `path`, a root that holds the marker of this global data directory's
    /// root ID and generation, or any marker where `external-root` is missing, and move nothing.
    pub adopt: u8,
    /// Give up the unavailable current root for a new, empty one at `path`, and move nothing.
    pub abandon: u8,
}

/// Move every virtual instance's external directory to a new external root, `args.path`, and
/// name it in the setting, for the caller's global data directory.
///
/// Runs in the calling process. Stops the running service and holds its socket until the move
/// ends, then starts the service again. Refuses a service that runs inside another program
/// through `lore::service::run`, and a program that runs the service itself. `adopt` and
/// `abandon` change the setting and move nothing, and are refused together. The global
/// `dry_run` runs every check that needs nothing stopped and reports what the move would do.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn set_external_directory(
    globals: LoreGlobalArgs,
    args: LoreServiceSetExternalDirectoryArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Never relayed: the move stops the service a relay would reach.
    set_external_directory_local(globals, args, callback).await
}

fn set_external_directory_local(
    globals: LoreGlobalArgs,
    args: LoreServiceSetExternalDirectoryArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    set_external_directory_until(globals, args, callback, std::future::pending())
}

/// [`set_external_directory`], with `termination`, which cancels the move: completed before the
/// final switch, it undoes the move, and later, it takes effect once the move has finished.
pub fn set_external_directory_until(
    globals: LoreGlobalArgs,
    args: LoreServiceSetExternalDirectoryArgs,
    callback: LoreEventCallback,
    termination: impl Future<Output = ()> + 'static,
) -> impl Future<Output = i32> {
    let (dry_run, force) = (globals.dry_run(), globals.force());
    let working_directory = globals.working_directory().map(PathBuf::from);
    no_repository_call(
        globals,
        callback,
        args,
        set_external_directory,
        move |args: LoreServiceSetExternalDirectoryArgs| {
            set_external_directory_with(args, dry_run, force, working_directory, termination)
        },
    )
}

/// Runs the move `args` asks for, with the globals' `dry_run` and `force`, relative paths against
/// `working_directory`, until `termination`.
#[cfg(any(target_os = "windows", target_family = "unix"))]
async fn set_external_directory_with(
    args: LoreServiceSetExternalDirectoryArgs,
    dry_run: bool,
    force: bool,
    working_directory: Option<PathBuf>,
    termination: impl Future<Output = ()>,
) -> Result<(), set_external_directory::SetExternalDirectoryError> {
    let form = match (args.adopt != 0, args.abandon != 0) {
        (true, true) => {
            return Err(InvalidArguments {
                reason: "adopt and abandon can't be given together".to_string(),
            }
            .into());
        }
        (true, false) => Form::Adopt,
        (false, true) => Form::Abandon,
        (false, false) => Form::Move,
    };
    set_external_directory::set_external_directory(
        args.path.as_str(),
        form,
        dry_run,
        force,
        working_directory,
        termination,
    )
    .await
}

/// Fails, as a target with no IPC runs no service to move the external directories of.
#[cfg(not(any(target_os = "windows", target_family = "unix")))]
async fn set_external_directory_with(
    _args: LoreServiceSetExternalDirectoryArgs,
    _dry_run: bool,
    _force: bool,
    _working_directory: Option<PathBuf>,
    _termination: impl Future<Output = ()>,
) -> Result<(), ServiceProcessError> {
    Err(ServiceUnavailable {
        reason: "OS doesn't support IPC".to_string(),
    }
    .into())
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(set_use_automatically_local)]
/// Arguments for setting whether commands are carried out by the Lore service.
pub struct LoreServiceSetUseAutomaticallyArgs {
    /// Carry out commands in the service rather than in the process that was run
    pub enabled: u8,
}

/// Set whether commands are carried out by the Lore service, for this machine.
///
/// Stored in the user-level global config, so the service stays in use for later
/// commands rather than for one command at a time.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn set_use_automatically(
    globals: LoreGlobalArgs,
    args: LoreServiceSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Runs where it was called for the same reason [`set_executable`] does: a
    // setter that routed would have to reach a service in order to be told not
    // to use one.
    set_use_automatically_local(globals, args, callback).await
}

async fn set_use_automatically_local(
    globals: LoreGlobalArgs,
    args: LoreServiceSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let dry_run = globals.dry_run();
    let command =
        async move |args: LoreServiceSetUseAutomaticallyArgs| -> Result<(), ServiceProcessError> {
            let (mut config, lock) = GlobalConfig::load_locked()
                .await
                .forward::<ServiceProcessError>("loading global config")?;

            let enabled = args.enabled != 0;
            config.service.use_automatically = enabled.then_some(true);
            if !dry_run {
                config
                    .save(lock)
                    .await
                    .forward::<ServiceProcessError>("saving global config")?;

                // This process may go on to make more calls, and it decided once whether
                // to relay them.
                forget_whether_service_is_in_use();
            }

            let tense = if dry_run { "would" } else { "will" };
            if enabled {
                lore_info!("Lore commands {tense} be carried out by the service");
            } else {
                lore_info!("Lore commands {tense} be carried out in the process that runs them");
            }
            // Reported after the line above rather than instead of it: the setting
            // was stored, and this says what it will and will not do on its own.
            report_settings_update_that_will_not_relay(&config);
            Ok(())
        };
    no_repository_call(globals, callback, args, set_use_automatically, command).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(status_local)]
/// Arguments for querying the Lore service status (no parameters).
pub struct LoreServiceStatusArgs {}

/// Query the Lore service status.
///
/// Reports whether the service is running and, if so, its metadata: binary path,
/// uptime, active connection count, SWFS mount count, process ID, whether it runs
/// inside another program, and the virtual instances it mounts with their global data
/// directories. Reports none running, with the root's state interrupted, while a move
/// of the external root holds the service socket. Reports the external root of the
/// caller's global data directory either way: its path, whether it can be used, and
/// the number of leftovers listed. Also drains any buffered service-level log messages.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Operation-Specific Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::ServiceStatus`](crate::interface::LoreEvent::ServiceStatus) | Service running state and metadata |
/// | [`LoreEvent::ServiceMessage`](crate::interface::LoreEvent::ServiceMessage) | Buffered service-level log message |
pub async fn status(
    globals: LoreGlobalArgs,
    args: LoreServiceStatusArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Status runs where it was called, like start and stop. When in the service
    // process, it queries local state. When in a client, it tries to connect.
    status_local(globals, args, callback).await
}

/// Reports the status of the service that runs in this process, from its own state.
fn status_here(
    globals: LoreGlobalArgs,
    args: LoreServiceStatusArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, status, move |_args| async move {
        emit_status_from_service(root_status().await?);
        Ok::<(), ServiceProcessError>(())
    })
}

async fn status_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStatusArgs,
    callback: LoreEventCallback,
) -> i32 {
    if service_runs_in_this_process() {
        return status_here(globals, args, callback).await;
    }

    // Running in a client process: try to connect to the service. No service runs while a move
    // of the external root holds the socket, so its status is reported as with none listening.
    let (globals, callback, moving) = match reach_running_service().await {
        Ok(Reached::Here) => return status_here(globals, args, callback).await,
        Ok(Reached::Connected(connection)) => {
            let relayed = run_service_call(Some(connection), globals, args.into(), callback).await;
            match relayed {
                Relayed::Ran(status) => return status,
                Relayed::HandedBack(call) => {
                    let HandedBack {
                        globals, callback, ..
                    } = *call;
                    (globals, callback, true)
                }
            }
        }
        Ok(Reached::Nothing) => (globals, callback, false),
        Err(error) => {
            return no_repository_call(globals, callback, args, status, move |_args| async move {
                Err::<(), ServiceProcessError>(error)
            })
            .await;
        }
    };
    no_repository_call(
        globals,
        callback,
        LoreServiceStatusArgs {},
        status,
        move |_args| async move {
            let mut root = root_status().await?;
            if moving {
                root.1 = LoreExternalRootState::Interrupted;
            }
            emit_not_running_status(root);
            Ok::<(), ServiceProcessError>(())
        },
    )
    .await
}

/// Emit `ServiceStatus` and `ServiceMessage` events from within the service process.
fn emit_status_from_service(root: RootStatus) {
    // The status command arrives through command dispatch, which carries no
    // service state, so this is one of the paths that reaches for the global.
    let service_state = ServiceStateImpl::global();

    // Check if service state is initialized.
    if !service_state.is_initialized() {
        // Shouldn't happen if wired correctly, but emit not-running as fallback.
        emit_not_running_status(root);
        return;
    }

    // Get binary path.
    let binary_path = std::env::current_exe().map_or_default(|p| p.display().to_string());

    let mount_manager = MountManagerState::mount_manager();
    let swfs_mount_count = mount_manager
        .as_ref()
        .map_or(0, |m| m.swfs().mount_count() as u32);
    let (mounted_global_data_directories, mounted_paths) = mount_manager
        .as_deref()
        .map_or_else(Default::default, mounted);
    let (external_root, external_root_state, external_leftovers) = root;

    LoreEvent::ServiceStatus(LoreServiceStatusEventData {
        running: 1,
        binary_path: LoreString::from(binary_path.as_str()),
        uptime_ms: service_state.uptime_ms(),
        connection_count: service_state.connection_count(),
        swfs_mount_count,
        external_root,
        external_root_state,
        external_leftovers,
        embedded: u8::from(!serves_from_this_executable()),
        mounted_global_data_directories: LoreArray::from_vec(mounted_global_data_directories),
        mounted_paths: LoreArray::from_vec(mounted_paths),
        process_id: std::process::id(),
    })
    .send();

    // Drain buffered logs and emit ServiceMessage events.
    let (messages, dropped_count) = service_state.drain_logs();
    for message in messages {
        LoreEvent::ServiceMessage(LoreServiceMessageEventData {
            level: message.level,
            message: LoreString::from(message.message.as_str()),
        })
        .send();
    }

    // If messages were dropped, emit a synthetic error message.
    if dropped_count > 0 {
        LoreEvent::ServiceMessage(LoreServiceMessageEventData {
            level: LoreLogLevel::Error,
            message: LoreString::from(
                format!("{dropped_count} service log messages were dropped due to buffer overflow")
                    .as_str(),
            ),
        })
        .send();
    }
}

/// Emit a `ServiceStatus` event indicating the service is not running, with the external root of
/// the caller's global data directory.
fn emit_not_running_status(root: RootStatus) {
    let (external_root, external_root_state, external_leftovers) = root;
    LoreEvent::ServiceStatus(LoreServiceStatusEventData {
        running: 0,
        binary_path: LoreString::default(),
        uptime_ms: 0,
        connection_count: 0,
        swfs_mount_count: 0,
        external_root,
        external_root_state,
        external_leftovers,
        embedded: 0,
        mounted_global_data_directories: LoreArray::default(),
        mounted_paths: LoreArray::default(),
        process_id: 0,
    })
    .send();
}

/// The path, state and leftover count of the caller's external root, as a status reports them.
type RootStatus = (LoreString, LoreExternalRootState, u32);

/// [`external_root_status`], read off the runtime.
async fn root_status() -> Result<RootStatus, ServiceProcessError> {
    Ok(lore_base::lore_spawn_blocking!(external_root_status)
        .await
        .internal("reading the external root")?)
}

/// The path, state and leftover count of the caller's external root, as a status reports them;
/// an empty path that can't be used where there is no global data directory. Blocking.
fn external_root_status() -> RootStatus {
    let Ok(state) = external_dir::resolve::resolve() else {
        return (LoreString::default(), LoreExternalRootState::Unavailable, 0);
    };
    let leftovers = Leftovers::load(&state.global).map_or(0, |listed| {
        u32::try_from(listed.paths.len() + listed.marked.len()).unwrap_or(u32::MAX)
    });
    (
        LoreString::from(reported_root(&state).to_string_lossy().as_ref()),
        (&state.availability).into(),
        leftovers,
    )
}

/// The canonical path of each global data directory `mount_manager` mounts virtual instances for,
/// and the path of each mount.
fn mounted(mount_manager: &MountManager) -> (Vec<LoreString>, Vec<LoreString>) {
    let mut globals: Vec<LoreString> = Vec::new();
    let mut paths = Vec::new();
    for (path, _, _) in mount_manager.mounts() {
        if let Ok(Some(record)) = mount_manager.record_at(&path) {
            let global = record.global.to_string_lossy();
            if !globals.iter().any(|known| known.as_str() == global) {
                globals.push(LoreString::from(global.as_ref()));
            }
        }
        paths.push(LoreString::from(path.to_string_lossy().as_ref()));
    }
    (globals, paths)
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default)]
/// Arguments for running the Lore service in the current process (no parameters).
///
/// Does not derive `LoreArgs`: running the service is always local, as it is the
/// service that handles IPC and so cannot itself be routed through it.
pub struct LoreServiceRunArgs {}

/// Run the Lore service in the current process.
///
/// Returns once the service has stopped, either by `lore service stop` over IPC
/// or by [`quit`] from within this process.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn run(
    globals: LoreGlobalArgs,
    args: LoreServiceRunArgs,
    callback: LoreEventCallback,
) -> i32 {
    let service_globals = globals.clone();
    no_repository_call(globals, callback, args, run, async move |_args| {
        service_main(service_globals, Arc::clone(ServiceStateImpl::global())).await
    })
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default)]
/// Arguments for requesting the Lore service running in this process to quit (no parameters).
pub struct LoreServiceQuitArgs {}

/// Request the Lore service running in this process to quit.
///
/// This is an in-process cancellation mechanism: it asks a service started with
/// [`run`] in this process to shut down cleanly, and does not reach a service in
/// another process (use [`stop`] for that). When no service runs in this
/// process, this is a no-op that returns success.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn quit(
    globals: LoreGlobalArgs,
    args: LoreServiceQuitArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Like [`stop`], this acts where it was called: routed through the service
    // it would start one just to be told to quit.
    quit_local(globals, args, callback).await
}

async fn quit_local(
    globals: LoreGlobalArgs,
    args: LoreServiceQuitArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, quit, move |_args| async move {
        request_service_stop();
        Ok::<(), ServiceProcessError>(())
    })
    .await
}
