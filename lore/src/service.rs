// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod initialization;
pub mod service_main;

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::global::GlobalConfig;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore_info;
use lore_revision::lore_warn;
use lore_revision::util::config::SaveableConfig;
use serde::Deserialize;
use serde::Serialize;

use crate::call::no_repository_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreString;
use crate::remote::call::service_call_over;
use crate::remote::service_process::ServiceProcessError;
use crate::remote::service_process::collect_exited_services;
use crate::remote::service_process::connect_or_spawn_service;
use crate::remote::service_process::connect_to_running_service;
use crate::remote::service_process::forget_whether_service_is_in_use;
use crate::remote::service_process::report_settings_update_that_will_not_relay;
use crate::remote::service_process::request_service_stop;
use crate::remote::service_process::service_runs_in_this_process;
use crate::remote::service_process::wait_until_no_service_is_listening;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(start_local)]
/// Arguments for starting the Lore service process (no parameters).
pub struct LoreServiceStartArgs {}

/// Start the Lore service process, unless one is already running.
///
/// Connects to the running service, and starts one when nothing is listening.
/// Succeeds once a service is reachable, whether it was already running or was
/// started here.
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

async fn start_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, start, move |_args| async move {
        if service_runs_in_this_process() {
            // The command reached the service itself, which is running, and a
            // start asks for nothing more than that.
            return Ok(());
        }

        // The connection is proof that a service is reachable and nothing more.
        // A start asks for one to be running, not for anything to be sent to it,
        // so it is closed again here.
        connect_or_spawn_service().await.map(|_connection| {
            lore_info!("Lore service is running");
        })
    })
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(stop_local)]
/// Arguments for stopping the Lore service process.
pub struct LoreServiceStopArgs {}

/// Stop the running Lore service process.
///
/// Does not start a service to stop, and succeeds when none is running, since
/// that is the state it asks for.
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

async fn stop_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> i32 {
    if service_runs_in_this_process() {
        return no_repository_call(globals, callback, args, stop, move |_args| async move {
            request_service_stop();
            Ok::<(), ServiceProcessError>(())
        })
        .await;
    }

    // Reached in the process the caller ran. Hand the command to the service so
    // that the branch above runs there, over a connection made here: a stop acts
    // on a service that is running and must not be what starts one.
    match connect_to_running_service().await {
        Ok(Some(connection)) => {
            let status = service_call_over(connection, globals, args, callback).await;
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
        Ok(None) => {
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
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
    let command =
        async move |args: LoreServiceSetExecutableArgs| -> Result<(), ServiceProcessError> {
            let (mut config, lock) = GlobalConfig::load_locked()
                .await
                .forward::<ServiceProcessError>("loading global config")?;

            // Blank clears rather than stores, matching how a blank value already
            // reads as unset everywhere it is resolved.
            let executable = args.executable.as_str().trim();
            config.service.executable = (!executable.is_empty()).then(|| executable.to_string());
            match config.service_executable() {
                Some(executable) => lore_info!("Lore service executable set to {executable}"),
                None => lore_info!("Lore service executable cleared"),
            }
            // Clearing the executable is one of the two ways to end up with a pair
            // that does not relay.
            report_settings_update_that_will_not_relay(&config);

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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
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
    let command =
        async move |args: LoreServiceSetUseAutomaticallyArgs| -> Result<(), ServiceProcessError> {
            let (mut config, lock) = GlobalConfig::load_locked()
                .await
                .forward::<ServiceProcessError>("loading global config")?;

            let enabled = args.enabled != 0;
            config.service.use_automatically = enabled.then_some(true);
            config
                .save(lock)
                .await
                .forward::<ServiceProcessError>("saving global config")?;

            // This process may go on to make more calls, and it decided once whether
            // to relay them.
            forget_whether_service_is_in_use();

            if enabled {
                lore_info!("Lore commands will be carried out by the service");
            } else {
                lore_info!("Lore commands will be carried out in the process that runs them");
            }
            // Reported after the line above rather than instead of it: the setting
            // was stored, and this says what it will and will not do on its own.
            report_settings_update_that_will_not_relay(&config);
            Ok(())
        };
    no_repository_call(globals, callback, args, set_use_automatically, command).await
}
