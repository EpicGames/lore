// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod run;

use clap::Args;
use clap::Subcommand;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
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
pub struct ServiceStopArgs {
    /// Flag to stop servicing all repositories
    #[clap(value_name = "all")]
    all: Option<bool>,
}

#[derive(Subcommand)]
pub enum ServiceCommands {
    ///Run this process as the service
    Run(ServiceRunArgs),

    /// Start service for a repository
    Start(ServiceStartArgs),

    /// Stop service for a repository
    Stop(ServiceStopArgs),
}

fn handle_service_run(_globals: LoreGlobalArgs, _args: &ServiceRunArgs) -> u8 {
    match runtime().block_on(async move { service_main(None).await }) {
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

fn handle_service_start(globals: LoreGlobalArgs, _args: &ServiceStartArgs) -> u8 {
    let start_args = LoreServiceStartArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(service::start(globals, start_args, callback)) as u8;
}

fn handle_service_stop(globals: LoreGlobalArgs, args: &ServiceStopArgs) -> u8 {
    let stop_args = LoreServiceStopArgs {
        all: if args.all.unwrap_or_default() { 1 } else { 0 },
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(service::stop(globals, stop_args, callback)) as u8;
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
    }
}
