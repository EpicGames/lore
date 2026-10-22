// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use clap::Args;
use clap::Subcommand;
use lore::call_delegation::run_command_locally;
use lore::interface::LoreEvent;
use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreLogLevel;
use lore::interface::LoreServiceMessageEventData;
use lore::interface::LoreServiceSetExecutableArgs;
use lore::interface::LoreServiceSetUseAutomaticallyArgs;
use lore::interface::LoreServiceStartArgs;
use lore::interface::LoreServiceStatusArgs;
use lore::interface::LoreServiceStatusEventData;
use lore::interface::LoreServiceStopArgs;
use lore::runtime;
use lore::service;
use lore::service::LoreServiceQuitArgs;
use lore::service::LoreServiceRunArgs;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::eprintln;
use crate::println;
use crate::styling::CommonStyles;
use crate::styling::LogStyles;
use crate::util;
use crate::util::TerminationSignals;

#[derive(Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommands,
}

#[derive(Args)]
pub struct ServiceRunArgs {}

#[derive(Args)]
pub struct ServiceStartArgs {}

#[derive(Args)]
pub struct ServiceStopArgs {}

#[derive(Args)]
pub struct ServiceStatusArgs {}

#[derive(Args)]
pub struct ServiceSetExecutableArgs {
    /// Path of the executable to start as the service. Leave empty to clear it
    #[clap(value_name = "path")]
    executable: Option<String>,
}

#[derive(Args)]
pub struct ServiceSetUseAutomaticallyArgs {
    /// Whether to carry commands out in the service
    ///
    /// `Set` rather than the default a `bool` field is given: this reads a value
    /// rather than being present or absent, and clap refuses a positional whose
    /// action takes none.
    #[clap(value_name = "enabled", action = clap::ArgAction::Set)]
    enabled: bool,
}

#[derive(Subcommand)]
pub enum ServiceCommands {
    ///Run this process as the service
    Run(ServiceRunArgs),

    /// Start the service, unless one is already running
    Start(ServiceStartArgs),

    /// Stop the running service
    Stop(ServiceStopArgs),

    /// Report whether the service is running, and what it is doing
    Status(ServiceStatusArgs),

    /// Set which executable is started as the service
    SetExecutable(ServiceSetExecutableArgs),

    /// Set whether commands are carried out by the service
    SetUseAutomatically(ServiceSetUseAutomaticallyArgs),
}

/// Where the service process parks its working directory. It inherits one from
/// whoever started it, which is unrelated to the directories its callers run in,
/// and holding that directory would also keep the filesystem under it busy.
/// Callers send the directory their relative paths belong to, so the service
/// never needs one of its own.
///
/// Done here and not in `service::run`, so that a program embedding the service
/// keeps its own working directory.
fn detached_working_directory() -> std::path::PathBuf {
    #[cfg(target_family = "unix")]
    {
        std::path::PathBuf::from("/")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("SystemRoot").map_or_else(
            || std::path::PathBuf::from("C:\\"),
            std::path::PathBuf::from,
        )
    }
}

fn handle_service_run(globals: LoreGlobalArgs, _args: &ServiceRunArgs) -> u8 {
    let detached = detached_working_directory();
    if let Err(error) = std::env::set_current_dir(&detached) {
        eprintln!(
            "{}Failed to set working directory to {}:{} {error}",
            CommonStyles::FAILURE,
            detached.display(),
            anstyle::Reset
        );
    }

    runtime().block_on(async move {
        // Registered before the service starts, because a signal ends the process
        // outright until a handler is in place: registering after the service
        // binds its socket leaves a window in which a service callers can already
        // reach dies rather than stopping, leaving the socket behind.
        let signals = match TerminationSignals::register() {
            Ok(signals) => Some(signals),
            Err(error) => {
                eprintln!(
                    "{}Failed to listen for termination signals:{} {error}",
                    CommonStyles::FAILURE,
                    anstyle::Reset
                );
                // Without signals the service is still stoppable over IPC
                None
            }
        };

        let run = service::run(globals.clone(), LoreServiceRunArgs {}, service_callback());
        tokio::pin!(run);

        let Some(mut signals) = signals else {
            return run.await as u8;
        };

        // The service ends by itself when stopped over IPC or when it fails to start,
        // and by a quit when a signal arrives first.
        // Biased towards the service, so that it is polled and has begun to start
        // by the time a signal that arrived already is acted on: a quit before
        // that has no service to reach.
        tokio::select! {
            biased;
            status = &mut run => status as u8,
            () = signals.recv() => {
                service::quit(globals, LoreServiceQuitArgs {}, service_callback()).await;
                run.await as u8
            }
        }
    })
}

/// The default handlers, which put errors on stderr, plus the maintenance
/// notices a server can send. `Complete` is swallowed, for the commands that
/// say their own outcome. The JSON formatter replaces it wholesale when that
/// output mode is on.
fn service_callback() -> LoreEventCallback {
    output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ))
}

fn handle_service_start(globals: LoreGlobalArgs, _args: &ServiceStartArgs) -> u8 {
    let start_args = LoreServiceStartArgs {};

    return run_command_locally(globals, start_args.into(), service_callback()) as u8;
}

fn handle_service_stop(globals: LoreGlobalArgs, _args: &ServiceStopArgs) -> u8 {
    let stop_args = LoreServiceStopArgs {};

    return run_command_locally(globals, stop_args.into(), service_callback()) as u8;
}

/// Reads an uptime in milliseconds as the span an operator would say out loud:
/// the largest unit it reaches, and every unit below that one down to seconds.
///
/// Units below the largest are kept even at zero so that two readings of the
/// same magnitude line up against each other, and seconds are truncated rather
/// than rounded so that a service is never reported as older than it is.
fn format_uptime(uptime_ms: u64) -> String {
    let total_seconds = uptime_ms / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = (total_seconds / (60 * 60)) % 24;
    // Not wrapped at any point above: days is what a long-lived service
    // accumulates in, so it carries the whole of the rest.
    let days = total_seconds / (60 * 60 * 24);

    if days > 0 {
        format!("{days}d {hours}h {minutes}m {seconds}s")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Reports what the service is, where it is, and how busy it is. Nothing is
/// printed beyond the first line when none is running: the fields are zeroed in
/// that case, and printing them would read as a service that is running and
/// idle.
fn print_service_status(data: &LoreServiceStatusEventData) {
    if data.running == 0 {
        println!("No Lore service is running");
        return;
    }

    println!("Lore service is running");
    println!(
        "  {}Executable:{} {}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        data.binary_path.as_str()
    );
    println!(
        "  {}Uptime:{} {}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        format_uptime(data.uptime_ms)
    );
    println!(
        "  {}Connections:{} {}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        data.connection_count
    );
    println!(
        "  {}SWFS mounts:{} {}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        data.swfs_mount_count
    );
}

/// Reads a buffered service message as a line, naming the level it came in at.
///
/// `Info` is left bare, as this client's own logs are: it is the level the
/// service writes when nothing is wrong, and a tag on every line would bury the
/// ones whose level is worth reading.
fn format_service_message(level: LoreLogLevel, message: &str) -> String {
    if level == LoreLogLevel::Info {
        return message.to_string();
    }

    format!("[{level}] {message}")
}

/// Prints a log message the service buffered while no command was running.
///
/// Styled as this client's own logs are, so that the service's output reads the
/// same wherever it is seen. Unfiltered, and on stdout whatever the level: these
/// are the answer to the command that asked for them rather than diagnostics
/// about it, so a level that would suppress or redirect this client's own logs
/// would be suppressing the output that was asked for.
fn print_service_message(data: &LoreServiceMessageEventData) {
    println!(
        "{}{}{}",
        LogStyles::from_level(data.level),
        format_service_message(data.level, data.message.as_str()),
        anstyle::Reset
    );
}

fn handle_service_status(globals: LoreGlobalArgs, _args: &ServiceStatusArgs) -> u8 {
    let status_args = LoreServiceStatusArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::ServiceStatus(data) => print_service_status(data),
            LoreEvent::ServiceMessage(data) => print_service_message(data),
            LoreEvent::Maintenance(data) => util::handle_maintenance_event(data),
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(service::status(globals, status_args, callback)) as u8;
}

fn handle_service_set_executable(globals: LoreGlobalArgs, args: &ServiceSetExecutableArgs) -> u8 {
    let set_args = LoreServiceSetExecutableArgs {
        executable: args.executable.clone().unwrap_or_default().into(),
    };

    return run_command_locally(globals, set_args.into(), service_callback()) as u8;
}

fn handle_service_set_use_automatically(
    globals: LoreGlobalArgs,
    args: &ServiceSetUseAutomaticallyArgs,
) -> u8 {
    let set_args = LoreServiceSetUseAutomaticallyArgs {
        enabled: u8::from(args.enabled),
    };

    return run_command_locally(globals, set_args.into(), service_callback()) as u8;
}

pub fn handle_service_commands(cmd: &ServiceCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        ServiceCommands::Run(args) => {
            return handle_service_run(globals, args);
        }
        ServiceCommands::Start(args) => {
            return handle_service_start(globals, args);
        }
        ServiceCommands::Stop(args) => {
            return handle_service_stop(globals, args);
        }
        ServiceCommands::Status(args) => {
            return handle_service_status(globals, args);
        }
        ServiceCommands::SetExecutable(args) => {
            return handle_service_set_executable(globals, args);
        }
        ServiceCommands::SetUseAutomatically(args) => {
            return handle_service_set_use_automatically(globals, args);
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use lore::interface::LoreLogLevel;

    use super::ServiceCommands;
    use super::format_service_message;
    use super::format_uptime;
    use crate::cli::LoreCli;
    use crate::cli::LoreCommands;

    /// Info is what the service logs when nothing is wrong, and a level on every
    /// line would bury the ones that carry a level worth reading.
    #[test]
    fn an_info_message_is_reported_as_the_service_wrote_it() {
        assert_eq!(
            format_service_message(LoreLogLevel::Info, "Lore service listening"),
            "Lore service listening"
        );
    }

    /// Every other level is named, because a status that buffered a warning and
    /// one that buffered an error read alike otherwise.
    #[test]
    fn a_levelled_message_is_reported_with_its_level() {
        for (level, expected) in [
            (LoreLogLevel::Trace, "[Trace] mounting"),
            (LoreLogLevel::Debug, "[Debug] mounting"),
            (LoreLogLevel::Warn, "[Warn] mounting"),
            (LoreLogLevel::Error, "[Error] mounting"),
        ] {
            assert_eq!(format_service_message(level, "mounting"), expected);
        }
    }

    /// The buffer holds whatever the service put in it, including nothing.
    #[test]
    fn an_empty_message_keeps_its_level() {
        assert_eq!(format_service_message(LoreLogLevel::Error, ""), "[Error] ");
    }

    /// A service that has just bound its socket has an uptime below the
    /// resolution reported, and rounding that down to nothing would read as no
    /// uptime at all rather than as a service that has only just started.
    #[test]
    fn uptime_below_a_second_reads_as_zero_seconds() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(999), "0s");
    }

    #[test]
    fn uptime_truncates_to_whole_seconds() {
        assert_eq!(format_uptime(1_000), "1s");
        assert_eq!(format_uptime(1_999), "1s");
        assert_eq!(format_uptime(59_000), "59s");
    }

    /// Units above seconds appear only once they are reached, and everything
    /// below the largest one appears with them, so that the reading is the same
    /// width whatever the value inside a given magnitude.
    #[test]
    fn uptime_carries_into_larger_units() {
        assert_eq!(format_uptime(60_000), "1m 0s");
        assert_eq!(format_uptime(3_599_000), "59m 59s");
        assert_eq!(format_uptime(3_600_000), "1h 0m 0s");
        assert_eq!(format_uptime(86_399_000), "23h 59m 59s");
        assert_eq!(format_uptime(86_400_000), "1d 0h 0m 0s");
        assert_eq!(format_uptime(90_061_000), "1d 1h 1m 1s");
    }

    /// A service left running for years is still one value, so the largest unit
    /// accumulates rather than wrapping.
    #[test]
    fn uptime_accumulates_in_the_largest_unit() {
        assert_eq!(format_uptime(1000 * 60 * 60 * 24 * 400), "400d 0h 0m 0s");
    }

    #[test]
    fn status_is_a_service_subcommand_taking_no_arguments() {
        let cli = LoreCli::try_parse_from(["lore", "service", "status"])
            .expect("`lore service status` must parse");

        let Some(LoreCommands::Service(service)) = cli.command else {
            panic!("`service status` must parse as a service command");
        };
        assert!(
            matches!(service.command, ServiceCommands::Status(_)),
            "`service status` must parse as the status subcommand"
        );
    }
}
