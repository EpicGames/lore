// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use clap::ArgAction;
use clap::Args;
use clap::Subcommand;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreSharedStoreCreateArgs;
use lore::interface::LoreSharedStoreInfoArgs;
use lore::interface::LoreSharedStoreSetUseAutomaticallyArgs;
use lore::interface::LoreString;
use lore::runtime;
use lore::shared_store;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::println;
use crate::styling::CommonStyles;

#[derive(Args)]
pub struct SharedStoreArgs {
    /// Store subcommand
    #[command(subcommand)]
    pub command: SharedStoreCommands,
}

#[derive(Subcommand)]
pub enum SharedStoreCommands {
    /// Create a shared store backed by a remote
    Create(SharedStoreCreateArgs),

    /// Show the shared store this repository uses
    Info(SharedStoreInfoArgs),

    /// Set whether new clones use a shared store without being asked to
    SetUseAutomatically(SharedStoreSetUseAutomaticallyArgs),
}

#[derive(Args)]
pub struct SharedStoreCreateArgs {
    /// Remote URL that will back the store
    #[clap(value_name = "remote-url")]
    remote_url: String,

    /// Where to create the shared store
    #[clap(long, value_name = "path")]
    path: Option<String>,

    /// Set this as the default shared store in the global config file, defaults to true
    #[clap(long, default_missing_value = "true")]
    make_default: Option<bool>,
}

#[derive(Args)]
pub struct SharedStoreInfoArgs {}

#[derive(Args)]
pub struct SharedStoreSetUseAutomaticallyArgs {
    /// Whether to automatically use the shared store
    #[clap(value_name = "enabled", value_parser = clap::value_parser!(bool), action = ArgAction::Set)]
    enabled: bool,
}

pub fn handle_store_commands(command: &SharedStoreCommands, globals: LoreGlobalArgs) -> u8 {
    match command {
        SharedStoreCommands::Create(args) => handle_create(globals, args),

        SharedStoreCommands::Info(args) => handle_info(globals, args),

        SharedStoreCommands::SetUseAutomatically(args) => {
            handle_set_use_automatically(globals, args)
        }
    }
}

pub fn handle_create(globals: LoreGlobalArgs, args: &SharedStoreCreateArgs) -> u8 {
    let args = LoreSharedStoreCreateArgs {
        remote_url: LoreString::from(&args.remote_url),
        path: args.path.as_ref().map_or("", |s| s as &str).into(),
        make_default: args.make_default.unwrap_or(true) as u8,
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(|event: &LoreEvent| {
            if let LoreEvent::SharedStoreCreate(data) = event {
                println!("Created shared store in {}", data.path.as_str(),);
            }
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(shared_store::create(globals, args, callback)) as u8
}

fn display_bool(value: u8) -> &'static str {
    if value != 0 { "true" } else { "false" }
}

pub fn handle_info(globals: LoreGlobalArgs, _args: &SharedStoreInfoArgs) -> u8 {
    let args = LoreSharedStoreInfoArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(|event: &LoreEvent| {
            if let LoreEvent::SharedStoreInfo(data) = event {
                println!(
                    "Shared store will be used automatically: {}",
                    display_bool(data.use_automatically)
                );
                for i in 0..data
                    .remote_urls
                    .len()
                    .min(data.paths.len())
                    .min(data.exists.len())
                {
                    println!(
                        "{}Remote URL:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.remote_urls.as_slice()[i]
                    );
                    println!(
                        "  {}Path:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.paths.as_slice()[i]
                    );
                    println!(
                        "  {}Exists:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        display_bool(data.exists.as_slice()[i])
                    );
                }
            }
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(shared_store::info(globals, args, callback)) as u8
}

pub fn handle_set_use_automatically(
    globals: LoreGlobalArgs,
    args: &SharedStoreSetUseAutomaticallyArgs,
) -> u8 {
    let args = LoreSharedStoreSetUseAutomaticallyArgs {
        enabled: args.enabled as u8,
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(|_event: &LoreEvent| {}) as EventCallbackFn).with_defaults(),
    ));

    runtime().block_on(shared_store::set_use_automatically(globals, args, callback)) as u8
}
