// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use chrono::DateTime;
use clap::Args;
use clap::Subcommand;
use lore::auth;
use lore::auth::LoreAuthUserInfoArgs;
use lore::interface::LoreArray;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreLockFileAcquireArgs;
use lore::interface::LoreLockFileReleaseArgs;
use lore::interface::LoreLockFileStatusArgs;
use lore::interface::LoreString;
use lore::lock;
use lore::lock::LoreLockFileQueryArgs;
use lore::runtime;
use parking_lot::Mutex;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::println;
use crate::styling::CommonStyles;
use crate::styling::LogStyles;
use crate::util;
use crate::util::convert_to_lore_string_vec;

#[derive(Args)]
pub struct LockFileArgs {
    #[command(subcommand)]
    pub command: LockFileCommands,
}

#[derive(Subcommand)]
pub enum LockFileCommands {
    /// Acquire lock on file(s)
    Acquire(FileLockAcquireArgs),

    /// Get lock status on file(s)
    Status(FileLockStatusArgs),

    /// Query the lock status given a branch, owner or path
    Query(FileLockQueryArgs),

    /// Release lock on file(s)
    Release(FileLockReleaseArgs),
}

#[derive(Args)]
#[group(required = true)]
pub struct FileLockAcquireArgs {
    /// Any number of file paths to lock
    #[clap(value_name = "paths", num_args=1..)]
    paths: Vec<String>,
    /// Branch where lock is to be acquired
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
}

#[derive(Args)]
pub struct FileLockStatusArgs {
    /// Any number of file paths to get the lock status
    #[clap(value_name = "paths", num_args=1..)]
    paths: Vec<String>,
    /// Branch where lock was acquired
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
}

#[derive(Args)]
pub struct FileLockQueryArgs {
    /// Branch to query locks on
    #[arg(long, value_name = "branch-name")]
    branch: Option<String>,
    /// Owner to query locks belonging to them
    #[arg(long, value_name = "owner-id")]
    owner: Option<String>,
    /// Path to query lock information on
    #[arg(long, value_name = "path")]
    path: Option<String>,
}

#[derive(Args)]
pub struct FileLockReleaseArgs {
    /// Any number of file paths to release the lock
    #[clap(value_name = "paths", num_args=1..)]
    paths: Vec<String>,
    /// Branch where lock was acquired
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
    /// Owner of the lock
    #[clap(long, value_name = "owner")]
    owner: Option<String>,
}

fn handle_lock_acquire(globals: LoreGlobalArgs, args: &FileLockAcquireArgs) -> u8 {
    let paths = convert_to_lore_string_vec(&args.paths);

    let acquire_args = LoreLockFileAcquireArgs {
        paths: LoreArray::from_vec(paths),
        branch: LoreString::from(&args.branch),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LockFileAcquireBegin(data) if data.count > 0 => {
                let header = if data.ignored != 0 {
                    "Lock already owned on files:"
                } else if globals.dry_run != 0 {
                    "Lock would be acquired on files:"
                } else {
                    "Lock acquired on files:"
                };
                println!("{}{}{}", CommonStyles::HEADERS, header, anstyle::Reset);
            }
            LoreEvent::LockFileAcquire(data) => {
                println!("{}", data.path.as_str());
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(lock::file_acquire(globals, acquire_args, callback)) as u8;
}

struct LockEventData {
    branch: String,
    path: String,
    owner: String,
    timestamp: String,
}

fn handle_lock_status(globals: LoreGlobalArgs, args: &FileLockStatusArgs) -> u8 {
    let paths = convert_to_lore_string_vec(&args.paths);

    let status_args = LoreLockFileStatusArgs {
        paths: LoreArray::from_vec(paths),
        branch: LoreString::from(&args.branch),
    };

    let status_data: Arc<Mutex<Vec<LockEventData>>> = Arc::new(Mutex::new(Vec::default()));

    let status_data_clone = status_data.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LockFileStatusBegin(data) if data.count > 0 => {
                println!(
                    "{}Files locked for edit:{}",
                    CommonStyles::HEADERS,
                    anstyle::Reset
                );
            }
            LoreEvent::LockFileStatus(data) => {
                let timestamp = parse_timestamp(data.locked_at);

                status_data_clone.lock().push(LockEventData {
                    branch: String::default(),
                    path: data.path.to_string(),
                    owner: data.owner.to_string(),
                    timestamp,
                });
            }
            LoreEvent::PathIgnore(data) => {
                util::handle_path_ignore_event(data);
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let result_status =
        runtime().block_on(lock::file_status(globals.clone(), status_args, callback)) as u8;

    let auth_data = resolve_user_ids(globals, status_data.clone());

    let status_data = status_data.lock();
    let auth_data = auth_data.lock();
    for data in status_data.iter() {
        let owner = auth_data.get(&data.owner).unwrap_or(&data.owner);
        println!("{} by {} on {}", data.path, owner, data.timestamp);
    }

    result_status
}

fn handle_lock_query(globals: LoreGlobalArgs, args: &FileLockQueryArgs) -> u8 {
    let query_args = LoreLockFileQueryArgs {
        branch: LoreString::from(&args.branch),
        owner: LoreString::from(&args.owner),
        path: LoreString::from(&args.path),
    };

    let query_data: Arc<Mutex<Vec<LockEventData>>> = Arc::new(Mutex::new(Vec::default()));

    let query_data_clone = query_data.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LockFileQueryBegin(_data) => {
                println!("{}Locks found:{}", CommonStyles::HEADERS, anstyle::Reset);
            }
            LoreEvent::LockFileQuery(data) => {
                let timestamp = parse_timestamp(data.locked_at);

                query_data_clone.lock().push(LockEventData {
                    branch: data.branch.to_string(),
                    path: data.path.to_string(),
                    owner: data.owner.to_string(),
                    timestamp,
                });
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let result_query =
        runtime().block_on(lock::file_query(globals.clone(), query_args, callback)) as u8;

    let auth_data = resolve_user_ids(globals, query_data.clone());
    let query_data = query_data.lock();
    let auth_data = auth_data.lock();
    for data in query_data.iter() {
        let owner = auth_data.get(&data.owner).unwrap_or(&data.owner);
        println!("{} by {} on branch {}", data.path, owner, data.branch);
    }

    result_query
}

fn handle_lock_release(globals: LoreGlobalArgs, args: &FileLockReleaseArgs) -> u8 {
    let paths = convert_to_lore_string_vec(&args.paths);

    let release_args = LoreLockFileReleaseArgs {
        paths: LoreArray::from_vec(paths),
        branch: LoreString::from(&args.branch),
        owner: LoreString::from(&args.owner),
        owner_id: LoreString::default(),
    };

    let globals = globals.clone();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LockFileReleaseBegin(data) => {
                if data.not_found != 0 {
                    println!(
                        "{}Lock does not exist for requested files{}",
                        LogStyles::WARNING,
                        anstyle::Reset
                    );
                } else if data.count > 0 {
                    let header = if globals.dry_run != 0 {
                        "Lock would be released on files:"
                    } else {
                        "Lock released on files:"
                    };
                    println!("{}{}{}", CommonStyles::HEADERS, header, anstyle::Reset);
                }
            }
            LoreEvent::LockFileRelease(data) => {
                println!("{}", data.path.as_str());
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(lock::file_release(globals, release_args, callback)) as u8;
}

pub fn handle_lock_file_commands(globals: LoreGlobalArgs, cmd: &LockFileCommands) -> u8 {
    match cmd {
        LockFileCommands::Acquire(args) => handle_lock_acquire(globals, args),
        LockFileCommands::Status(args) => handle_lock_status(globals, args),
        LockFileCommands::Query(args) => handle_lock_query(globals, args),
        LockFileCommands::Release(args) => handle_lock_release(globals, args),
    }
}

fn parse_timestamp(timestamp: u64) -> String {
    DateTime::from_timestamp_millis(timestamp as i64)
        .map(|time| time.to_rfc2822())
        .unwrap_or_default()
}

fn resolve_user_ids(
    globals: LoreGlobalArgs,
    data: Arc<Mutex<Vec<LockEventData>>>,
) -> Arc<Mutex<HashMap<String, String>>> {
    let user_ids = data
        .lock()
        .iter()
        .map(|data| LoreString::from(&data.owner))
        .collect();

    let auth_args = LoreAuthUserInfoArgs {
        user_ids: LoreArray::from_vec(user_ids),
    };

    let auth_data: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::default()));

    let auth_data_clone = auth_data.clone();
    // Sub-operation callback; safe to ignore error events.
    let callback =
        output_formatter().unwrap_or(Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthUserInfo(data) => {
                auth_data_clone
                    .lock()
                    .insert(data.id.to_string(), data.name.to_string());
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        })));

    let _result = runtime().block_on(auth::resolve_user_info(
        globals.clone(),
        auth_args,
        callback,
    )) as u8;

    auth_data
}
