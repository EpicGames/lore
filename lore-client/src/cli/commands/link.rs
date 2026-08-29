// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use clap::Args;
use clap::Subcommand;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreLinkChangeEventData;
use lore::interface::LoreLinkEntryEventData;
use lore::interface::LoreString;
use lore::link;
use lore::link::LinkFlags;
use lore::link::LoreLinkAddArgs;
use lore::link::LoreLinkInfoArgs;
use lore::link::LoreLinkListArgs;
use lore::link::LoreLinkRemoveArgs;
use lore::link::LoreLinkStagedState;
use lore::link::LoreLinkUpdateArgs;
use lore::runtime;
use parking_lot::Mutex;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::commands::revision;
use crate::eprintln;
use crate::println;
use crate::progress_bar::ProgressBar;
use crate::progress_bar::progress_debug;
use crate::styling::CommonStyles;
use crate::styling::FileActionStyle;
use crate::util;

#[derive(Args)]
pub struct LinkArgs {
    #[command(subcommand)]
    pub command: LinkCommands,
}

#[derive(Args)]
pub struct LinkAddArgs {
    /// Path in the current repository where the repository should be linked in
    #[clap(value_name = "link_path")]
    link_path: String,

    /// Repository URL
    #[clap(value_name = "link_url")]
    link: String,

    /// Path in the link repository that should be linked in
    #[clap(value_name = "source_path")]
    source_path: String,

    /// Branch or specific revision to pin the link to, defaulting to latest on the main branch
    #[clap(long, value_name = "pin")]
    pin: Option<String>,

    /// Disable automatic branch creation in the linked repository
    #[clap(long, action)]
    disable_branching: bool,
}

#[derive(Args)]
pub struct LinkRemoveArgs {
    /// Path in the current repository where the module is linked in
    #[clap(value_name = "link_path")]
    link_path: String,
}

#[derive(Args)]
pub struct LinkUpdateArgs {
    /// Path in the repository where the link should be updated
    #[clap(value_name = "link_path")]
    link_path: String,

    /// Branch or specific revision to pin the link to, defaulting to latest on the current branch
    #[clap(long, value_name = "pin")]
    pin: Option<String>,
}

#[derive(Args)]
pub struct LinkInfoArgs {
    /// Path in the repository of the link to describe
    #[clap(value_name = "link_path")]
    link_path: String,
}

#[derive(Args)]
pub struct LinkListArgs {
    /// Only show links with staged changes
    #[clap(long, action)]
    staged: bool,
}

#[derive(Subcommand)]
pub enum LinkCommands {
    /// Link to the given point in the repository and subpath from the given repository
    Add(LinkAddArgs),

    /// Remove the link at the given point in the repository
    Remove(LinkRemoveArgs),

    /// Update the link to a new pin
    Update(LinkUpdateArgs),

    /// List all links in the repository
    List(LinkListArgs),

    /// Show detailed information about the link at the given path
    Info(LinkInfoArgs),
}

fn handle_link_add(globals: LoreGlobalArgs, args: &LinkAddArgs) -> u8 {
    let repository_identifier = if !args.link.contains("/") {
        let Ok(mut url) = std::env::var("LORE_REMOTE_URL") else {
            eprintln!("Link URL must include a host name");
            return 1;
        };
        url.push('/');
        url.push_str(args.link.as_str());
        url
    } else {
        args.link.clone()
    };

    let link_args = LoreLinkAddArgs {
        link: LoreString::from(&repository_identifier),
        link_path: LoreString::from(&args.link_path),
        source_path: LoreString::from(&args.source_path),
        pin: args.pin.as_ref().into(),
        disable_branching: args.disable_branching as u8,
    };

    let start = std::time::Instant::now();
    let bar = ProgressBar::new_spinner("Cloning ...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryCloneBegin(data) => {
                println!(
                    "Cloning repository {} branch {} into {}",
                    data.repository, data.branch, data.path
                );
            }
            LoreEvent::RepositoryCloneProgress(data) => {
                crate::progress_bar::clone::apply_clone_progress(
                    data.count.file_count,
                    data.count.file_complete,
                    data.count.bytes_transferred,
                    data.count.bytes_total,
                    data.count.discovery_complete,
                    &bar,
                );
            }
            LoreEvent::RepositoryCloneEnd(data) => {
                println!(
                    "Cloned {}/{} files ({}/{})",
                    data.count.file_complete,
                    data.count.file_count,
                    crate::util::format_bytes_to_string(data.count.bytes_transferred),
                    crate::util::format_bytes_to_string(data.count.bytes_total),
                );
                println!("Clone complete in {:.2}s", start.elapsed().as_secs_f32());
            }
            LoreEvent::LinkChange(data) => {
                println!(
                    "{}Added link and staged for commit{}",
                    CommonStyles::SUCCESS,
                    anstyle::Reset
                );
                print_link_pin(data);
                print_link_change(data);
            }
            LoreEvent::Complete(data) if data.status != 0 => {
                println!(
                    "{}Failed to add link{}",
                    CommonStyles::FAILURE,
                    anstyle::Reset
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(link::add(globals, link_args, callback)) as u8;
}

fn handle_link_remove(globals: LoreGlobalArgs, args: &LinkRemoveArgs) -> u8 {
    let unlink_args = LoreLinkRemoveArgs {
        link_path: LoreString::from(&args.link_path),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LinkChange(data) => {
                println!(
                    "{}Removed link and staged for commit{}",
                    CommonStyles::SUCCESS,
                    anstyle::Reset,
                );
                print_link_change(data);
            }
            LoreEvent::Complete(data) if data.status != 0 => {
                println!(
                    "{}Failed to remove link{}",
                    CommonStyles::FAILURE,
                    anstyle::Reset
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(link::remove(globals, unlink_args, callback)) as u8;
}

fn format_link_staged_state(state: LoreLinkStagedState) -> &'static str {
    match state {
        LoreLinkStagedState::None => "none",
        LoreLinkStagedState::Added => "added",
        LoreLinkStagedState::Removed => "removed",
        LoreLinkStagedState::Modified => "modified",
    }
}

/// Prints the rows every link listing shares. `link info` adds its own below.
fn print_link_entry(data: &LoreLinkEntryEventData, branch_name: &str) {
    println!(
        "{}Link {}{}",
        CommonStyles::HEADERS,
        data.link,
        anstyle::Reset
    );
    println!(
        "  {}Link path:{} {} (node {})",
        CommonStyles::HEADERS,
        anstyle::Reset,
        data.link_path,
        data.link_node
    );
    println!(
        "  {}Source path:{} {} (node {})",
        CommonStyles::HEADERS,
        anstyle::Reset,
        data.source_path,
        data.source_node
    );
    let branch_id = data.branch.to_string();
    let branch = if branch_name.is_empty() || branch_name == branch_id {
        branch_id
    } else {
        format!("{branch_name} ({branch_id})")
    };
    println!(
        "  {}Branch:{} {} [{}]",
        CommonStyles::HEADERS,
        anstyle::Reset,
        branch,
        if data.tracking != 0 {
            "tracking"
        } else {
            "pinned"
        }
    );
    println!(
        "  {}Revision:{} {}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        data.revision
    );
    println!(
        "  {}Flags:{} {}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        format_link_flags(data.flags)
    );
}

fn handle_link_info(globals: LoreGlobalArgs, args: &LinkInfoArgs) -> u8 {
    let info_args = LoreLinkInfoArgs {
        link_path: LoreString::from(&args.link_path),
    };

    // Collected rather than printed in the callback: naming the branch is
    // another interface call, which cannot run while this one is in flight.
    let info = Arc::new(Mutex::new(None));
    let info_cb = info.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LinkInfo(data) => {
                *info_cb.lock() = Some(data.clone());
            }
            LoreEvent::Complete(data) if data.status != 0 => {
                eprintln!("Failed to read link info");
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let status = runtime().block_on(link::info(globals.clone(), info_args, callback)) as u8;

    if let Some(data) = info.lock().take() {
        let mut branches = util::BranchNameResolver::new(globals);
        let branch_name = branches.name(data.entry.branch, data.entry.link_path.as_str());
        print_link_entry(&data.entry, &branch_name);
        if !data.remote_revision.is_zero() {
            println!(
                "  {}Remote revision:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                data.remote_revision
            );
        }
        println!(
            "  {}Staged:{} {}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            format_link_staged_state(data.staged_state)
        );
        println!(
            "  {}Staged files:{} {}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            data.staged_file_count
        );
    }

    return status;
}

fn handle_link_list(globals: LoreGlobalArgs, args: &LinkListArgs) -> u8 {
    if args.staged {
        return handle_link_list_staged(globals);
    }

    let list_args = LoreLinkListArgs {};

    // Collected rather than printed in the callback: naming the branch is
    // another interface call, which cannot run while this one is in flight.
    let entries = Arc::new(Mutex::new(Vec::new()));
    let entries_cb = entries.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LinkEntry(data) => {
                entries_cb.lock().push(data.clone());
            }
            LoreEvent::Complete(data) if data.status != 0 => {
                eprintln!("Failed to list links");
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let status = runtime().block_on(link::list(globals.clone(), list_args, callback)) as u8;

    let entries = entries.lock().split_off(0);
    if entries.is_empty() {
        println!("No links found in this repository");
        return status;
    }

    let mut branches = util::BranchNameResolver::new(globals);
    for entry in &entries {
        let branch_name = branches.name(entry.branch, entry.link_path.as_str());
        print_link_entry(entry, &branch_name);
        println!("");
    }

    return status;
}

fn handle_link_list_staged(globals: LoreGlobalArgs) -> u8 {
    use lore::interface::LoreEventCallback;

    let discovered_links: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::default()));
    let discovered_links_clone = discovered_links.clone();
    let callback: LoreEventCallback = Some(
        (Box::new(move |event: &LoreEvent| {
            if let LoreEvent::LinkStagedEntry(data) = event {
                discovered_links_clone
                    .lock()
                    .push((data.path.to_string(), data.staged_file_count));
            }
        }) as EventCallbackFn)
            .with_defaults(),
    );

    runtime().block_on(lore::link::list_staged(globals, callback));

    let links = discovered_links.lock();
    if links.is_empty() {
        println!("No linked repositories with staged changes");
    } else {
        for (path, file_count) in links.iter() {
            println!(
                "{}{}{} ({} file{} changed)",
                CommonStyles::SUCCESS,
                path,
                anstyle::Reset,
                file_count,
                if *file_count == 1 { "" } else { "s" }
            );
        }
    }

    0
}

fn handle_link_update(globals: LoreGlobalArgs, args: &LinkUpdateArgs) -> u8 {
    let debug = progress_debug();

    let update_args = LoreLinkUpdateArgs {
        link_path: LoreString::from(&args.link_path),
        pin: args.pin.as_ref().into(),
    };

    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LinkChange(data) => {
                if data.branch.is_zero() && data.revision.is_zero() {
                    println!("Link is already up to date");
                } else {
                    println!(
                        "{}Updated link and staged for commit{}",
                        CommonStyles::SUCCESS,
                        anstyle::Reset,
                    );
                    print_link_pin(data);
                    print_link_change(data);
                }
            }
            LoreEvent::Complete(data) => {
                if data.status != 0 {
                    println!(
                        "{}Failed to update link{}",
                        CommonStyles::FAILURE,
                        anstyle::Reset
                    );
                }
            }
            _ => revision::handle_sync_event(event, &progress_bar, debug),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(link::update(globals, update_args, callback)) as u8;
}

fn print_link_pin(data: &LoreLinkChangeEventData) {
    println!(
        "{}Branch:{}{} {}{}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        CommonStyles::DEFAULT,
        data.branch,
        anstyle::Reset
    );
    println!(
        "{}Revision:{}{} {}{}",
        CommonStyles::HEADERS,
        anstyle::Reset,
        CommonStyles::DEFAULT,
        data.revision,
        anstyle::Reset
    );
}

fn print_link_change(data: &LoreLinkChangeEventData) {
    let mut link_path = data.link_path.to_string();
    link_path.push('/');

    println!(
        "{}{}{} {}",
        FileActionStyle::from_action(data.action),
        data.action.as_string_short(),
        anstyle::Reset,
        link_path
    );
}

fn format_link_flags(flags: u32) -> String {
    let flags = LinkFlags::from_bits_truncate(flags);
    let name = if flags.is_empty() {
        "None".to_string()
    } else {
        let mut names = vec![];
        if flags.contains(LinkFlags::DisableAutoFollow) {
            names.push("DisableAutoFollow");
        }
        names.join(", ")
    };
    format!("{name} ({:#x})", flags.bits())
}

pub fn handle_link_commands(cmd: &LinkCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        LinkCommands::Add(args) => {
            return handle_link_add(globals, args);
        }
        LinkCommands::Remove(args) => {
            return handle_link_remove(globals, args);
        }
        LinkCommands::Update(args) => {
            return handle_link_update(globals, args);
        }
        LinkCommands::List(args) => {
            return handle_link_list(globals, args);
        }
        LinkCommands::Info(args) => {
            return handle_link_info(globals, args);
        }
    }
}
