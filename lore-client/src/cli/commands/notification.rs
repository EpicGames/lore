// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use clap::Args;
use clap::Subcommand;
use lore::call_delegation::run_command;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreNotificationSubscribeArgs;
use lore::interface::LoreNotificationUnsubscribeArgs;
use lore::runtime;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::println;
use crate::util;

#[derive(Args)]
pub struct NotificationArgs {
    #[command(subcommand)]
    pub command: NotificationCommands,
}

#[derive(Args)]
pub struct SubscribeArgs {
    /// Time to be subscribed in seconds
    #[clap(value_name = "seconds")]
    timeout: Option<u32>,
}

#[derive(Subcommand)]
pub enum NotificationCommands {
    /// Subscribe to events on the given repository
    Subscribe(SubscribeArgs),
}

fn handle_notification_subscribe(globals: LoreGlobalArgs, args: &SubscribeArgs) -> u8 {
    let timeout = args.timeout;

    let args = LoreNotificationSubscribeArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::NotificationSubscribed(data) => {
                println!("Subscribed to events from repository {}", data.repository);
            }
            LoreEvent::NotificationUnsubscribed(data) => {
                println!("Unsubscribed to events from repository {}", data.repository);
            }
            LoreEvent::NotificationBranchPushed(data) => {
                println!(
                    "Branch pushed by {}: {} revision {} -> {}",
                    data.user_id, data.branch, data.revision_number, data.revision
                );
            }
            LoreEvent::NotificationBranchCreated(data) => {
                println!("Branch created: {}", data.branch);
            }
            LoreEvent::NotificationBranchDeleted(data) => {
                println!("Branch deleted: {}", data.branch);
            }
            LoreEvent::NotificationResourceLocked(data) => {
                for path in data.paths.as_slice() {
                    println!("Resource locked by {}: {}", data.user_id, path);
                }
            }
            LoreEvent::NotificationResourceUnlocked(data) => {
                for path in data.paths.as_slice() {
                    println!("Resource unlocked by {}: {}", data.user_id, path);
                }
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    println!("Subscribing to notifications...");

    let result = run_command(globals.clone(), args.into(), callback) as u8;
    if result != 0 {
        return result;
    }

    if let Some(timeout) = timeout {
        println!("Listening for notifications for {timeout}s, press Ctrl+C to quit");
    } else {
        println!("Listening for notifications, press Ctrl+C to quit");
    }

    let _ = runtime().block_on(util::listen_for_termination(
        timeout.map(|t| Duration::from_secs(t as u64)),
    ));

    let args = LoreNotificationUnsubscribeArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| {
            if let LoreEvent::NotificationUnsubscribed(data) = event {
                println!("Unsubscribed from repository {}", data.repository);
            }
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    run_command(globals, args.into(), callback);

    return result;
}

pub fn handle_notification_commands(cmd: &NotificationCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        NotificationCommands::Subscribe(args) => handle_notification_subscribe(globals, args),
    }
}
