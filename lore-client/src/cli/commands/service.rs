// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod run;

use clap::Args;
use clap::Subcommand;
use lore::interface::LoreEvent;
use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreServiceSetExecutableArgs;
use lore::interface::LoreServiceSetUseAutomaticallyArgs;
use lore::interface::LoreServiceStartArgs;
use lore::interface::LoreServiceStopArgs;
use lore::runtime;
use lore::service;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::commands::service::run::service_main;
use crate::eprintln;
use crate::styling::CommonStyles;
use crate::util;

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

    /// Set which executable is started as the service
    SetExecutable(ServiceSetExecutableArgs),

    /// Set whether commands are carried out by the service
    SetUseAutomatically(ServiceSetUseAutomaticallyArgs),
}

fn handle_service_run(globals: LoreGlobalArgs, _args: &ServiceRunArgs) -> u8 {
    match runtime().block_on(async move { service_main(globals, None).await }) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!(
                "{}Error running service:{} {error}",
                CommonStyles::FAILURE,
                anstyle::Reset
            );
            1
        }
    }
}

/// What every `service` command reports through: the default handlers, which put
/// errors on stderr, and the maintenance notices a server can send. `Complete`
/// is swallowed because each of these commands says its own outcome. The JSON
/// formatter replaces it wholesale when that output mode is on.
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

    return runtime().block_on(service::start(globals, start_args, service_callback())) as u8;
}

fn handle_service_stop(globals: LoreGlobalArgs, _args: &ServiceStopArgs) -> u8 {
    let stop_args = LoreServiceStopArgs {};

    return runtime().block_on(service::stop(globals, stop_args, service_callback())) as u8;
}

fn handle_service_set_executable(globals: LoreGlobalArgs, args: &ServiceSetExecutableArgs) -> u8 {
    let set_args = LoreServiceSetExecutableArgs {
        executable: args.executable.clone().unwrap_or_default().into(),
    };

    return runtime().block_on(service::set_executable(
        globals,
        set_args,
        service_callback(),
    )) as u8;
}

fn handle_service_set_use_automatically(
    globals: LoreGlobalArgs,
    args: &ServiceSetUseAutomaticallyArgs,
) -> u8 {
    let set_args = LoreServiceSetUseAutomaticallyArgs {
        enabled: u8::from(args.enabled),
    };

    return runtime().block_on(service::set_use_automatically(
        globals,
        set_args,
        service_callback(),
    )) as u8;
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
        ServiceCommands::SetExecutable(args) => {
            return handle_service_set_executable(globals, args);
        }
        ServiceCommands::SetUseAutomatically(args) => {
            return handle_service_set_use_automatically(globals, args);
        }
    }
}
