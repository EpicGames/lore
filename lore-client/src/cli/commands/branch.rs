// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use clap::Args;
use clap::Subcommand;
use lore::branch;
use lore::branch::LoreBranchLatestListArgs;
use lore::branch::LoreBranchMetadataClearArgs;
use lore::branch::LoreBranchMetadataGetArgs;
use lore::branch::LoreBranchMetadataSetArgs;
use lore::interface::Context;
use lore::interface::LoreArray;
use lore::interface::LoreBranchArchiveArgs;
use lore::interface::LoreBranchCreateArgs;
use lore::interface::LoreBranchDiffArgs;
use lore::interface::LoreBranchInfoArgs;
use lore::interface::LoreBranchListArgs;
use lore::interface::LoreBranchLocation;
use lore::interface::LoreBranchMergeAbortArgs;
use lore::interface::LoreBranchMergeIntoArgs;
use lore::interface::LoreBranchMergeResolveArgs;
use lore::interface::LoreBranchMergeResolveMineArgs;
use lore::interface::LoreBranchMergeResolveTheirsArgs;
use lore::interface::LoreBranchMergeRestartArgs;
use lore::interface::LoreBranchMergeStartArgs;
use lore::interface::LoreBranchMergeUnresolveArgs;
use lore::interface::LoreBranchProtectArgs;
use lore::interface::LoreBranchPushArgs;
use lore::interface::LoreBranchResetArgs;
use lore::interface::LoreBranchSwitchArgs;
use lore::interface::LoreBranchUnprotectArgs;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreMetadataType;
use lore::interface::LoreString;
use lore::runtime;
use parking_lot::Mutex;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::commands::auth;
use crate::println;
use crate::progress_bar::ProgressBar;
use crate::progress_bar::progress_debug;
use crate::progress_bar::sync::apply_sync_progress_to_bar;
use crate::styling::BranchStyles;
use crate::styling::CommonStyles;
use crate::styling::FileActionStyle;
use crate::styling::LogStyles;
use crate::util;
use crate::util::convert_paths_and_targets;
use crate::util::format_bytes_to_string;
use crate::util::merge_result_display;
use crate::util::progress_info_display;

#[derive(Args)]
pub struct BranchCreateArgs {
    /// Name of the branch
    #[clap(value_name = "branch")]
    name: String,

    /// Optional explicit branch ID (hex-encoded 16-byte identifier)
    #[clap(long = "id", value_name = "id")]
    id: Option<String>,
}

#[derive(Args)]
pub struct BranchInfoArgs {
    /// Name of the branch
    #[clap(value_name = "branch")]
    name: Option<String>,
}

#[derive(Args)]
pub struct BranchPushArgs {
    /// Optional name or identifier of the branch, push current branch if not specified
    #[clap(value_name = "branch")]
    name: Option<String>,

    /// Allow the server to fast-forward merge if the target branch head has moved
    #[clap(long)]
    fast_forward_merge: bool,
}

#[derive(Args)]
pub struct BranchSwitchArgs {
    /// Name of the branch
    #[clap(value_name = "branch")]
    name: String,

    /// Revision to switch to
    #[clap(value_name = "revision")]
    revision: Option<String>,

    /// Do a dry run sync and only report what changes would be done, do not change anything in the file system
    #[clap(long, action)]
    dry_run: bool,

    /// Keep last local latest revision, do not sync latest revision from remote (implied by offline mode)
    #[clap(long, action)]
    local: bool,

    /// Reset any local modified files to match the incoming revision
    #[clap(long, action)]
    reset: bool,

    /// Only update anchor tracking without modifying or verifying files, useful for bare repositories
    #[clap(long, action)]
    bare: bool,
}

#[derive(Clone, Args)]
#[group(required = true, multiple = false)]
pub struct BranchSourceSpecifier {
    /// Name of the source branch to merge into the current branch
    #[clap(value_name = "branch")]
    name: Option<String>,

    /// ID of the source branch to merge into the current branch
    #[clap(long, value_name = "branch-id", conflicts_with = "name")]
    id: Option<Context>,
}

impl From<&BranchSourceSpecifier> for LoreString {
    fn from(value: &BranchSourceSpecifier) -> Self {
        if let Some(name) = value.name.as_deref() {
            return name.into();
        }

        println!(
            "Use of --branch-id is deprecated, the branch name argument can now take an ID directly"
        );
        value.id.unwrap_or_default().to_string().into()
    }
}

#[derive(Args)]
#[command(subcommand_negates_reqs = true)]
pub struct BranchMergeArgs {
    /// Merge action
    #[command(subcommand)]
    subcommand: Option<BranchMergeCommands>,

    /// Source branch to merge into the current branch
    #[clap(flatten)]
    branch: BranchSourceSpecifier,

    /// Change the message for committing when no conflicts arise from the merge
    #[clap(long, action)]
    message: Option<String>,
}

#[derive(Args)]
pub struct BranchMergeStartArgs {
    /// Source branch to merge into the current branch
    #[clap(flatten)]
    branch: BranchSourceSpecifier,

    /// Change the message for committing when no conflicts arise from the merge
    #[clap(long, action)]
    message: Option<String>,

    /// Disable auto commits even if no conflicts arise from the merge.
    #[clap(long, action)]
    no_commit: bool,

    /// Do a dry run merge start and only report what changes would be done, do not change anything in the file system
    #[clap(long, action)]
    dry_run: bool,

    /// Merge only a specific linked repository at the given mount path
    #[clap(long, conflicts_with = "ignore_links")]
    link: Option<String>,

    /// Merge only the main repository, skipping all linked repositories
    #[clap(long, action, conflicts_with = "link")]
    ignore_links: bool,
}

#[derive(Args)]
pub struct BranchMergeAbortArgs {
    /// Abort only a specific linked repository merge at the given mount path
    #[clap(long, conflicts_with = "ignore_links")]
    link: Option<String>,

    /// Abort only the main repository merge, keeping link pin updates
    #[clap(long, conflicts_with = "link")]
    ignore_links: bool,
}

#[derive(Clone, Args)]
#[group(required = true, multiple = false)]
pub struct BranchTargetSpecifier {
    /// Name of the target branch to merge the current branch into
    #[clap(value_name = "branch")]
    name: String,

    /// ID of the target branch to merge the current branch into
    #[clap(long, value_name = "branch-id", conflicts_with = "name")]
    id: Option<Context>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct BranchMergeUnresolveArgs {
    /// Any number of paths or files to unresolve
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
pub struct BranchMergeIntoArgs {
    /// Name of the branch to merge into
    #[clap(flatten)]
    branch: BranchTargetSpecifier,

    /// Commit message
    message: String,

    /// Merge only a specific linked repository at the given mount path
    #[clap(long, conflicts_with = "ignore_links")]
    link: Option<String>,

    /// Merge only the main repository, skipping all linked repositories
    #[clap(long, action, conflicts_with = "link")]
    ignore_links: bool,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct BranchMergeRestartArgs {
    /// Any number of paths or files to restart
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BranchMergeResolveArgs {
    /// Merge action
    #[command(subcommand)]
    subcommand: Option<BranchMergeResolveCommands>,

    /// Any number of paths or files to reset
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct BranchMergeResolveSubcommandArgs {
    /// Any number of paths or files to stage
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Subcommand)]
pub enum BranchMergeResolveCommands {
    /// Resolve using my changes
    Mine(BranchMergeResolveSubcommandArgs),

    /// Resolve using their changes
    Theirs(BranchMergeResolveSubcommandArgs),
}

#[derive(Args)]
pub struct BranchDiffArgs {
    /// Name of the source branch
    #[clap(long, value_name = "source")]
    source: Option<String>,

    /// Name of the target branch
    #[clap(value_name = "target")]
    target: String,

    /// Attempt to auto resolve conflicts if true
    #[clap(long, action)]
    auto_resolve: bool,
}

#[derive(Args)]
pub struct BranchProtectArgs {
    /// Name of the branch to protect
    #[clap(value_name = "branch")]
    branch: String,
}

#[derive(Args)]
pub struct BranchUnprotectArgs {
    /// Name of the branch to unprotect
    #[clap(value_name = "branch")]
    branch: String,
}

#[derive(Args)]
pub struct BranchArchiveArgs {
    /// Name of the branch to archive
    #[clap(value_name = "branch")]
    branch: String,
}

#[derive(Subcommand)]
pub enum BranchMergeCommands {
    /// Marks the merge unresolved
    Unresolve(BranchMergeUnresolveArgs),

    /// Merge into branch
    Into(BranchMergeIntoArgs),

    /// Start a merge process
    Start(BranchMergeStartArgs),

    /// Restart the merge, resetting the current merge state
    Restart(BranchMergeRestartArgs),

    /// Resolves the merge
    Resolve(BranchMergeResolveArgs),

    /// Abort a merge process
    Abort(BranchMergeAbortArgs),
}

#[derive(Args)]
pub struct BranchArgs {
    #[command(subcommand)]
    pub command: BranchCommands,
}

#[derive(Args)]
pub struct BranchListArgs {
    /// Include archived local branches
    #[clap(long)]
    archived: bool,
}

#[derive(Args)]
pub struct BranchResetArgs {
    /// Revision to reset the local latest pointer to
    #[clap(value_name = "revision")]
    revision: String,
    /// Branch to reset, or the current branch if not set
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
}

#[derive(Subcommand)]
pub enum BranchCommands {
    /// List available branches
    List(BranchListArgs),

    /// Get info about the given branch
    Info(BranchInfoArgs),

    /// Create a new branch
    Create(BranchCreateArgs),

    /// Switch to a different branch
    Switch(BranchSwitchArgs),

    /// Push commits to remote
    Push(BranchPushArgs),

    /// Merge two branches
    Merge(BranchMergeArgs),

    /// Diff two branches using the common ancestor base revision
    /// Will calculate the set of changes between source branch latest revision
    /// and the base revision that is not in the set of changes between the
    /// target branch latest revision and the base revision
    Diff(BranchDiffArgs),

    /// Archive an existing branch
    Archive(BranchArchiveArgs),

    /// Reset local latest pointer for a branch
    Reset(BranchResetArgs),

    // History,
    // Find,
    /// Protect a branch from direct pushes
    Protect(BranchProtectArgs),

    /// Remove push protection from a branch
    Unprotect(BranchUnprotectArgs),

    /// Branch latest related commands
    Latest(BranchLatestArgs),

    /// Branch metadata operations
    Metadata(BranchMetadataArgs),
}

#[derive(Args)]
pub struct BranchLatestArgs {
    /// List previous latest pointers of a branch
    #[command(subcommand)]
    subcommand: BranchLatestCommands,
}

#[derive(Subcommand)]
pub enum BranchLatestCommands {
    List(BranchLatestListArgs),
}

#[derive(Args)]
pub struct BranchLatestListArgs {
    /// Branch to query
    #[clap(long, value_name = "branch")]
    pub branch: Option<String>,

    /// Max number of history entries to show
    pub limit: Option<u32>,
}

#[derive(Args)]
pub struct BranchMetadataArgs {
    #[command(subcommand)]
    pub command: BranchMetadataCommands,
}

#[derive(Subcommand)]
pub enum BranchMetadataCommands {
    /// Get metadata from the branch (omit key to list all)
    Get(BranchMetadataGetArgs),

    /// Set metadata on the branch
    Set(BranchMetadataSetArgs),

    /// Clear metadata from the branch
    Clear(BranchMetadataClearArgs),
}

#[derive(Args)]
pub struct BranchMetadataGetArgs {
    /// Attribute to get (omit to list all)
    #[clap(value_name = "key")]
    key: Option<String>,

    /// Branch name (uses current branch if not specified)
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
}

#[derive(Args)]
pub struct BranchMetadataSetArgs {
    /// Metadata key/value pairs
    #[clap(value_name = "pairs", num_args = 1..)]
    pairs: Option<Vec<String>>,

    /// Indicator that values are paths to binary files
    #[clap(long, action)]
    binary: bool,

    /// Indicator that values are numeric (u64)
    #[clap(long, action, conflicts_with = "binary")]
    numeric: bool,

    /// Branch name (uses current branch if not specified)
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
}

#[derive(Args)]
pub struct BranchMetadataClearArgs {
    /// Keys to clear (omit to clear all user-defined keys)
    #[clap(value_name = "keys", num_args = 0..)]
    keys: Option<Vec<String>>,

    /// Branch name (uses current branch if not specified)
    #[clap(long, value_name = "branch")]
    branch: Option<String>,
}

fn handle_branch_latest_commands(globals: LoreGlobalArgs, args: &BranchLatestArgs) -> u8 {
    match &args.subcommand {
        BranchLatestCommands::List(args) => handle_branch_latest_list(globals, args),
    }
}

fn handle_branch_latest_list(globals: LoreGlobalArgs, args: &BranchLatestListArgs) -> u8 {
    let args = LoreBranchLatestListArgs {
        branch: args.branch.clone().into(),
        limit: args.limit.unwrap_or_default(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchLatestListEntry(data) => {
                println!("{}", data.revision);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));
    return runtime().block_on(branch::latest_list(globals, args, callback)) as u8;
}

fn handle_branch_create(globals: LoreGlobalArgs, args: &BranchCreateArgs) -> u8 {
    let _spinner = ProgressBar::new_spinner("Creating branch...");

    let branch = LoreString::from(&args.name);
    let category = LoreString::default();

    let id = LoreString::from(args.id.as_deref().unwrap_or_default());
    let create_args = LoreBranchCreateArgs {
        branch,
        category,
        id,
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchCreate(data) => {
                if data.is_commit == 0 {
                    println!(
                        "{}Created branch{} {}{}{} at revision {}",
                        CommonStyles::SUCCESS,
                        anstyle::Reset,
                        BranchStyles::CURRENT_BRANCH,
                        data.name.as_str(),
                        anstyle::Reset,
                        data.latest,
                    );
                } else {
                    println!(
                        "{}Created branch{} {}{}{} at new revision {} (updated for linked repositories)",
                        CommonStyles::SUCCESS,
                        anstyle::Reset,
                        BranchStyles::CURRENT_BRANCH,
                        data.name.as_str(),
                        anstyle::Reset,
                        data.latest,
                    );
                }
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::create(globals, create_args, callback)) as u8;
}

fn handle_branch_info(globals: LoreGlobalArgs, args: &BranchInfoArgs) -> u8 {
    let branch = LoreString::from(&args.name);

    let info_args = LoreBranchInfoArgs { branch };

    let description = Arc::new(Mutex::new(None));
    let description_cb = description.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchInfo(data) => {
                *description_cb.lock() = Some(data.clone());
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let status = runtime().block_on(branch::info(globals.clone(), info_args, callback)) as u8;

    if status != 0 {
        return status;
    }

    if let Some(data) = description.lock().clone() {
        println!(
            "{}Branch{} {}{}",
            CommonStyles::HEADERS,
            BranchStyles::CURRENT_BRANCH,
            data.name,
            anstyle::Reset
        );
        println!(
            "  {}ID:{} {}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            data.id
        );
        println!(
            "  {}Latest: {}{}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            data.latest
        );
        println!(
            "  {}Remote Latest: {}{}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            data.latest_remote
        );
        if !data.stack.is_empty() {
            for (index, entry) in data.stack.as_slice().iter().enumerate() {
                let name = resolve_branch_name(&globals, entry.branch);
                println!(
                    "  {}{}{}{}{} at {}",
                    CommonStyles::HEADERS,
                    if index == 0 { "Parent: " } else { "        " },
                    BranchStyles::CURRENT_BRANCH,
                    name,
                    anstyle::Reset,
                    entry.revision,
                );
            }
        }
        if !data.category.is_empty() {
            println!(
                "  {}Category: {}{}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                data.category
            );
        }
        if !data.creator.is_empty() {
            let names = auth::resolve_user_ids(globals, std::slice::from_ref(&data.creator));
            let creator = if let Some(creator) = names.get(&data.creator.to_string()) {
                creator.clone()
            } else {
                data.creator.to_string()
            };
            println!(
                "  {}Creator:{} {creator}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }

        if let Some(created) =
            DateTime::from_timestamp_millis(data.created as i64).map(|time| time.to_rfc2822())
        {
            println!(
                "  {}Created:{} {created}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
    }

    return status;
}

fn resolve_branch_name(globals: &LoreGlobalArgs, id: Context) -> String {
    let info_args = LoreBranchInfoArgs {
        branch: LoreString::from(id.to_string().as_str()),
    };
    let name = Arc::new(Mutex::new(None));
    let name_cb = name.clone();
    let callback: lore::interface::LoreEventCallback = Some(
        (Box::new(move |event: &LoreEvent| {
            if let LoreEvent::BranchInfo(data) = event {
                *name_cb.lock() = Some(data.name.to_string());
            }
        }) as EventCallbackFn)
            .with_defaults(),
    );
    runtime().block_on(branch::info(globals.clone(), info_args, callback));
    name.lock().take().unwrap_or(id.to_string())
}

fn handle_branch_switch(globals: LoreGlobalArgs, args: &BranchSwitchArgs) -> u8 {
    let switch_args = LoreBranchSwitchArgs {
        branch: LoreString::from(&args.name),
        revision: LoreString::from(&args.revision),
        reset: args.reset.into(),
        bare: args.bare.into(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchSwitchBegin(data) => match data.branch.location {
                LoreBranchLocation::Local => {
                    println!(
                        "Switching branch to {}{}{}, using current local latest revision {}",
                        BranchStyles::CURRENT_BRANCH,
                        data.branch.name,
                        anstyle::Reset,
                        data.branch.latest_local
                    );
                }
                LoreBranchLocation::Remote => {
                    println!(
                        "Switching branch to {}{}{}, using current remote latest revision {}",
                        BranchStyles::CURRENT_BRANCH,
                        data.branch.name,
                        anstyle::Reset,
                        data.branch.latest_remote
                    );
                }
            },
            LoreEvent::BranchSwitchEnd(data) => {
                println!(
                    "Switched to branch {}{}{} revision {}",
                    BranchStyles::CURRENT_BRANCH,
                    data.branch.name,
                    anstyle::Reset,
                    data.branch.revision
                );
            }
            LoreEvent::BranchMultipleInstance(data) => {
                println!(
                    "{}Warning: Branch {} is checked out in other instance(s):{}",
                    LogStyles::WARNING,
                    data.branch,
                    anstyle::Reset,
                );
                for (id, path) in data
                    .instance_ids
                    .as_slice()
                    .iter()
                    .zip(data.instance_paths.as_slice().iter())
                {
                    println!("  {}({id}) {path}{}", LogStyles::WARNING, anstyle::Reset);
                }
            }
            LoreEvent::RevisionResolve(data) => {
                if data.revision_number != 0 {
                    println!(
                        "Resolving revision number {} on branch {}",
                        data.revision_number, data.branch
                    );
                } else {
                    println!(
                        "Resolving revision partial hash signature {}",
                        data.revision
                    );
                }
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::switch(globals, switch_args, callback)) as u8;
}

pub fn handle_branch_push(globals: LoreGlobalArgs, args: &BranchPushArgs) -> u8 {
    let branch_name = Arc::new(Mutex::new(String::new()));
    let response_message = Arc::new(Mutex::new(String::new()));

    let push_args = LoreBranchPushArgs {
        branch: args.name.clone().into(),
        fast_forward_merge: args.fast_forward_merge.into(),
    };

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchPush(data) => {
                let main_branch_prints = data.branch_name.as_str() == "main"
                    && !data.remote_revision.is_zero()
                    && globals.force == 0;

                if data.remote_revision == data.local_revision {
                    if data.flag_link == 0 && data.flag_layer == 0 {
                        println!("Revision is already pushed and at remote latest");
                    }
                } else if data.local_history == 0 && main_branch_prints {
                    println!("Revision is already pushed and remote has moved forward");
                } else if data.remote_history == 0 && main_branch_prints {
                    println!(
                        "Local branch is {} revision(s) ahead of remote, pushing all revisions",
                        data.local_history
                    );
                    if data.flag_link != 0 {
                        println!("Linked repository {}", data.repository);
                    } else if data.flag_layer != 0 {
                        println!("Layer repository {}", data.repository);
                    } else {
                        println!("Repository {}", data.repository);
                    }
                }

                *branch_name.lock() = data.branch_name.to_string();
            }
            LoreEvent::BranchPushRevisionUpdateBegin(data) => {
                println!(
                    "Update revision {} from parent {} to new parent {}",
                    data.revision, data.old_parent, data.new_parent
                );
            }
            LoreEvent::BranchPushRevisionUpdateEnd(data) => {
                println!("Updated revision to {}", data.revision);
            }
            LoreEvent::BranchPushFragmentBegin(data) => {
                if data.fragments > 0 {
                    println!("Pushing {} fragment(s)", data.fragments);
                }
                progress_bar.set_max_progress(data.fragments);
            }
            LoreEvent::BranchPushFragmentProgress(data) => {
                if debug {
                    println!(
                        "[Debug] Push {}/{} fragment(s), {}/{}",
                        data.complete,
                        data.count,
                        format_bytes_to_string(data.bytes_transferred),
                        format_bytes_to_string(data.bytes_total)
                    );
                }
                progress_bar.set_max_progress(data.count);
                progress_bar.set_progress(data.complete);
            }
            LoreEvent::BranchPushFragmentEnd(data)
                if data.fragments > 0 => {
                    if data.bytes_transferred > 0 {
                        println!(
                            "Pushed {} fragment(s), {}",
                            data.fragments,
                            format_bytes_to_string(data.bytes_transferred)
                        );
                    } else {
                        println!("Pushed {} fragment(s)", data.fragments);
                    }
                }
            LoreEvent::BranchPushBranchCreateBegin(data) => {
                let branch_name = branch_name.lock();
                println!(
                    "Creating branch {} at {}",
                    *branch_name, data.local_revision
                );
            }
            LoreEvent::BranchPushRevisionPushBegin(data) => {
                let branch_name = branch_name.lock();
                if data.local_revision != data.remote_revision {
                    println!("Pushing {} to branch {}", data.local_revision, *branch_name);
                }
            }
            LoreEvent::BranchPushRevisionPushUpdate(data) => {
                println!(
                    "Revision assigned number {} and rewritten to {}",
                    data.new_revision_number, data.new_revision
                );
            }
            LoreEvent::BranchPushRevisionPushEnd(data) => {
                *response_message.lock() = data.message.to_string();
                let branch_name = branch_name.lock();
                if data.fast_forward_merged != 0 {
                    println!(
                        "Pushed revision {} -> {} to branch {} (fast-forward merged on server, run 'lore sync' to update)",
                        data.new_remote_revision_number, data.new_remote_revision, *branch_name
                    );
                } else if data.old_remote_revision != data.new_remote_revision {
                    println!(
                        "Pushed revision {} -> {} to branch {}",
                        data.new_remote_revision_number, data.new_remote_revision, *branch_name
                    );
                } else {
                    println!(
                        "Revision {} -> {} already at latest of branch {}",
                        data.new_remote_revision_number, data.new_remote_revision, *branch_name
                    );
                }
            }
            LoreEvent::Complete(data)
                if data.status == 0 => {
                    let message = response_message.lock();
                    if !message.is_empty() {
                        println!("{}", *message);
                    }
                }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::push(globals, push_args, callback)) as u8;
}

fn handle_branch_merge_unresolve(globals: LoreGlobalArgs, args: &BranchMergeUnresolveArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let merge_unresolve_args = LoreBranchMergeUnresolveArgs { paths };

    let count_atomic = AtomicU64::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchMergeUnresolveFile(data) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!(
                        "{}Marked unresolved:{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                }
                println!(
                    "{}{}{}",
                    BranchStyles::CONFLICT,
                    data.path.as_str(),
                    anstyle::Reset
                );

                count_atomic.fetch_add(1, Ordering::Relaxed);
            }
            LoreEvent::Complete(_) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!("No files marked as unresolved");
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::merge_unresolve(
        globals,
        merge_unresolve_args,
        callback,
    )) as u8;
}

fn handle_branch_merge_into(globals: LoreGlobalArgs, args: &BranchMergeIntoArgs) -> u8 {
    let branch = LoreString::from(&args.branch.name);
    let branch_id = args.branch.id.unwrap_or_default();
    let message = LoreString::from(&args.message);

    let merge_into_args = LoreBranchMergeIntoArgs {
        branch,
        branch_id,
        message,
        link: LoreString::from(&args.link),
        ignore_links: args.ignore_links as u8,
    };

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchMergeIntoFileBegin(data) => {
                println!(
                    "Found {} files and/or directories potentially affected by merge into",
                    data.count
                );
            }
            LoreEvent::BranchMergeIntoFile(data) => {
                println!("{}", data.path.as_str());
            }
            LoreEvent::BranchMergeIntoFragmentBegin(data) => {
                if data.fragments > 0 {
                    println!("Query {} fragment(s)", data.fragments);
                }
                progress_bar.set_max_progress(data.fragments);
            }
            LoreEvent::BranchMergeIntoFragmentProgress(data) => {
                if debug {
                    println!("[Debug] Push {}/{} fragment(s)", data.complete, data.count);
                }
                progress_bar.set_max_progress(data.count);
                progress_bar.set_progress(data.complete);
            }
            LoreEvent::BranchMergeIntoFragmentEnd(data) if data.fragments > 0 => {
                println!("\rPushed {} fragment(s)", data.fragments);
            }
            LoreEvent::BranchMergeIntoRevision(data) => {
                println!(
                    "{}Revision  :{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.revision_number
                );
                println!(
                    "{}Signature :{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.revision
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::merge_into(globals, merge_into_args, callback)) as u8;
}

fn handle_branch_merge_start(globals: LoreGlobalArgs, args: &BranchMergeStartArgs) -> u8 {
    let merge_start_args = LoreBranchMergeStartArgs {
        branch: LoreString::from(&args.branch),
        message: LoreString::from(&args.message),
        no_commit: args.no_commit as u8,
        link: LoreString::from(&args.link),
        ignore_links: args.ignore_links as u8,
    };

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchMergeStartBegin(_data) => {}
            LoreEvent::RevisionSyncProgress(data) => {
                apply_sync_progress_to_bar(&progress_bar, data);
                if debug {
                    println!("[Debug] {}", progress_info_display(data));
                }
            }
            LoreEvent::BranchMergeStartEnd(data) => {
                println!("\r{}", merge_result_display(&data.stats));
                println!("\rStaged merged repository state {}", data.signature);
                if data.has_conflicts == 1 {
                    println!(
                        "{}Files in conflict:{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                }
            }
            LoreEvent::RevisionCommitRevision(data) => {
                println!(
                    "Committed merged repository state {} -> {}",
                    data.revision_number, data.revision
                );
            }
            LoreEvent::BranchMergeConflictFile(data) => {
                println!("{}{}{}", BranchStyles::CONFLICT, data.path, anstyle::Reset);
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::merge_start(globals, merge_start_args, callback)) as u8;
}

fn handle_branch_merge_resolve(globals: LoreGlobalArgs, args: &BranchMergeResolveArgs) -> u8 {
    if let Some(subcommand) = &args.subcommand {
        // Resolve action
        match subcommand {
            BranchMergeResolveCommands::Mine(sub_args) => {
                handle_branch_merge_resolve_mine(globals, sub_args)
            }
            BranchMergeResolveCommands::Theirs(sub_args) => {
                handle_branch_merge_resolve_theirs(globals, sub_args)
            }
        }
    } else {
        let sub_args = BranchMergeResolveSubcommandArgs {
            paths: args.paths.clone(),
            targets: args.targets.clone(),
        };

        handle_branch_merge_resolve_impl(globals, &sub_args)
    }
}

fn handle_branch_merge_resolve_impl(
    globals: LoreGlobalArgs,
    args: &BranchMergeResolveSubcommandArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let merge_resolve_args = LoreBranchMergeResolveArgs { paths };

    let count_atomic = AtomicU64::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchMergeResolveFile(data) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!(
                        "{}Resolved conflicts:{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                }
                println!(
                    "{}{}{}",
                    CommonStyles::SUCCESS,
                    data.path.as_str(),
                    anstyle::Reset
                );

                count_atomic.fetch_add(1, Ordering::Relaxed);
            }
            LoreEvent::Complete(_) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!("No conflicts resolved");
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::merge_resolve(globals, merge_resolve_args, callback)) as u8;
}

fn handle_branch_merge_resolve_mine(
    globals: LoreGlobalArgs,
    args: &BranchMergeResolveSubcommandArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let merge_resolve_mine_args = LoreBranchMergeResolveMineArgs { paths };

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

    return runtime().block_on(branch::merge_resolve_mine(
        globals,
        merge_resolve_mine_args,
        callback,
    )) as u8;
}

fn handle_branch_merge_resolve_theirs(
    globals: LoreGlobalArgs,
    args: &BranchMergeResolveSubcommandArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let merge_resolve_theirs_args = LoreBranchMergeResolveTheirsArgs { paths };

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

    return runtime().block_on(branch::merge_resolve_theirs(
        globals,
        merge_resolve_theirs_args,
        callback,
    )) as u8;
}

fn handle_branch_merge_restart(globals: LoreGlobalArgs, args: &BranchMergeRestartArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let merge_restart_args = LoreBranchMergeRestartArgs { paths };

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

    return runtime().block_on(branch::merge_restart(globals, merge_restart_args, callback)) as u8;
}

fn handle_branch_merge_abort(globals: LoreGlobalArgs, args: &BranchMergeAbortArgs) -> u8 {
    let merge_abort_args = LoreBranchMergeAbortArgs {
        link: LoreString::from(&args.link),
        ignore_links: args.ignore_links as u8,
    };

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchMergeAbortBegin(data) => {
                println!(
                    "Merge abort from staged revision {} to current revision {}",
                    data.state_staged_revision, data.state_current_revision
                );
            }
            LoreEvent::RevisionSyncProgress(data) => {
                apply_sync_progress_to_bar(&progress_bar, data);
                if debug {
                    println!("[Debug] {}", progress_info_display(data));
                }
            }
            LoreEvent::BranchMergeAbortEnd(_data) => {
                println!("Merge abort reverted changes");
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::merge_abort(globals, merge_abort_args, callback)) as u8;
}

pub fn handle_branch_merge(globals: LoreGlobalArgs, args: &BranchMergeArgs) -> u8 {
    // Default action - start merge
    if args.subcommand.is_none() {
        let sub_args = BranchMergeStartArgs {
            branch: args.branch.clone(),
            message: args.message.clone(),
            no_commit: false,
            dry_run: false,
            link: None,
            ignore_links: false,
        };

        return handle_branch_merge_start(globals, &sub_args);
    }

    // Merge action
    match args.subcommand.as_ref().unwrap() {
        BranchMergeCommands::Unresolve(sub_args) => {
            return handle_branch_merge_unresolve(globals, sub_args);
        }

        BranchMergeCommands::Into(sub_args) => {
            return handle_branch_merge_into(globals, sub_args);
        }
        BranchMergeCommands::Start(sub_args) => {
            return handle_branch_merge_start(globals, sub_args);
        }
        BranchMergeCommands::Restart(sub_args) => {
            return handle_branch_merge_restart(globals, sub_args);
        }
        BranchMergeCommands::Abort(sub_args) => {
            return handle_branch_merge_abort(globals, sub_args);
        }
        BranchMergeCommands::Resolve(sub_args) => {
            return handle_branch_merge_resolve(globals, sub_args);
        }
    }
}

pub fn handle_branch_list(globals: LoreGlobalArgs, args: &BranchListArgs) -> u8 {
    let list_args = LoreBranchListArgs {
        archived: args.archived as u8,
    };

    let archived_section = std::sync::atomic::AtomicBool::new(false);
    let remote_seen = std::sync::atomic::AtomicBool::new(false);
    let warn_on_missing_remote = !globals.local() && !globals.remote() && !globals.offline();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchListBegin(data) => {
                if data.location == LoreBranchLocation::Remote {
                    remote_seen.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if data.location == LoreBranchLocation::Local
                    && archived_section.load(std::sync::atomic::Ordering::Relaxed)
                {
                    println!(
                        "{}Archived local branches:{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                } else {
                    println!(
                        "{}{}{}",
                        CommonStyles::HEADERS,
                        match data.location {
                            LoreBranchLocation::Local => "Local branches:",
                            LoreBranchLocation::Remote => "Remote branches:",
                        },
                        anstyle::Reset
                    );
                }
            }
            LoreEvent::BranchListEntry(data) => {
                println!(
                    "{}{} {}{}",
                    if data.archived != 0 {
                        BranchStyles::ARCHIVED
                    } else if data.is_current != 0 {
                        BranchStyles::CURRENT_BRANCH
                    } else {
                        CommonStyles::DEFAULT
                    },
                    if data.is_current != 0 { "*" } else { " " },
                    data.name,
                    anstyle::Reset,
                );
            }
            LoreEvent::BranchListEnd(data) => {
                if data.location == LoreBranchLocation::Local
                    && !archived_section.load(std::sync::atomic::Ordering::Relaxed)
                {
                    archived_section.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if data.count == 0 {
                    match data.location {
                        LoreBranchLocation::Local => println!("No local branches found"),
                        LoreBranchLocation::Remote => println!("No remote branches found"),
                    }
                }
            }
            LoreEvent::Complete(_) => {
                if warn_on_missing_remote && !remote_seen.load(std::sync::atomic::Ordering::Relaxed)
                {
                    println!(
                        "{}Warning: Could not query remote branch list{}",
                        LogStyles::WARNING,
                        anstyle::Reset,
                    );
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::list(globals, list_args, callback)) as u8;
}

pub fn handle_branch_diff(globals: LoreGlobalArgs, args: &BranchDiffArgs) -> u8 {
    let diff_args = LoreBranchDiffArgs {
        source: LoreString::from(&args.source),
        target: LoreString::from(&args.target),
        path: LoreString::default(),
        auto_resolve: args.auto_resolve.into(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchDiffBegin(_data) => {}
            LoreEvent::BranchDiffChangeBegin(data) => {
                println!(
                    "{}Found {} changes{}",
                    CommonStyles::HEADERS,
                    data.changes_count,
                    anstyle::Reset
                );
            }
            LoreEvent::BranchDiffChange(data) => {
                println!(
                    "{}{}{} {}{}",
                    FileActionStyle::from_action(data.change.action),
                    data.change.action.as_string_short(),
                    anstyle::Reset,
                    data.change.path.as_str(),
                    if data.change.automerged != 0 {
                        " (automerged)"
                    } else {
                        ""
                    }
                );
            }
            LoreEvent::BranchDiffChangeEnd(_data) => {}
            LoreEvent::BranchDiffConflictBegin(data) if data.conflicts_count > 0 => {
                println!(
                    "{}Found {} conflicts{}",
                    CommonStyles::HEADERS,
                    data.conflicts_count,
                    anstyle::Reset
                );
            }
            LoreEvent::BranchDiffConflict(data) => {
                println!(
                    "{}C{} {}{}",
                    BranchStyles::CONFLICT,
                    anstyle::Reset,
                    data.source_change.path.as_str(),
                    if data.source_change.automerged != 0 {
                        " (automerged)"
                    } else {
                        ""
                    }
                );
            }
            LoreEvent::BranchDiffConflictEnd(_data) => {}
            LoreEvent::BranchDiffEnd(_data) => {}
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::diff(globals, diff_args, callback)) as u8;
}

pub fn handle_branch_protect(globals: LoreGlobalArgs, args: &BranchProtectArgs) -> u8 {
    let protect_args = LoreBranchProtectArgs {
        branch: LoreString::from(&args.branch),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchProtect(data) => {
                println!(
                    "{}Branch {} protected{}",
                    CommonStyles::SUCCESS,
                    data.name,
                    anstyle::Reset
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::protect(globals, protect_args, callback)) as u8;
}

pub fn handle_branch_unprotect(globals: LoreGlobalArgs, args: &BranchUnprotectArgs) -> u8 {
    let unprotect_args = LoreBranchUnprotectArgs {
        branch: LoreString::from(&args.branch),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchUnprotect(data) => {
                println!(
                    "{}Branch {} unprotected{}",
                    CommonStyles::SUCCESS,
                    data.name,
                    anstyle::Reset
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::unprotect(globals, unprotect_args, callback)) as u8;
}

pub fn handle_branch_archive(globals: LoreGlobalArgs, args: &BranchArchiveArgs) -> u8 {
    let archive_args = LoreBranchArchiveArgs {
        branch: LoreString::from(&args.branch),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchArchive(data) => {
                println!(
                    "{}Archived branch {}{}",
                    CommonStyles::SUCCESS,
                    data.name,
                    anstyle::Reset
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::archive(globals, archive_args, callback)) as u8;
}

pub fn handle_branch_reset(globals: LoreGlobalArgs, args: &BranchResetArgs) -> u8 {
    let _spinner = ProgressBar::new_spinner("Resetting branch...");

    let reset_args = LoreBranchResetArgs {
        revision: LoreString::from(&args.revision),
        branch: args.branch.as_ref().into(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchReset(data) => {
                println!(
                    "{}Reset branch {} to {}{}",
                    CommonStyles::SUCCESS,
                    data.name,
                    data.revision,
                    anstyle::Reset
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(branch::reset(globals, reset_args, callback)) as u8;
}

fn resolve_branch_arg(branch: &Option<String>) -> LoreString {
    LoreString::from(branch.as_deref().unwrap_or(""))
}

pub fn handle_branch_metadata_get(globals: LoreGlobalArgs, args: &BranchMetadataGetArgs) -> u8 {
    let get_args = LoreBranchMetadataGetArgs {
        branch: resolve_branch_arg(&args.branch),
        key: LoreString::from(&args.key),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                super::file::print_metadata(data, None, None);
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(branch::metadata_get(globals, get_args, callback)) as u8
}

pub fn handle_branch_metadata_set(globals: LoreGlobalArgs, args: &BranchMetadataSetArgs) -> u8 {
    let format = if args.binary {
        LoreMetadataType::Binary
    } else if args.numeric {
        LoreMetadataType::Numeric
    } else {
        LoreMetadataType::String
    };

    let elements = convert_paths_and_targets(&args.pairs, &None);
    if !elements.as_slice().len().is_multiple_of(2) {
        println!(
            "error: metadata set requires <key> <value> pairs; each key must be followed by a value"
        );
        return 1;
    }

    let mut keys = vec![];
    let mut values = vec![];
    let mut formats = vec![];
    for (index, element) in elements.as_slice().iter().enumerate() {
        if index.is_multiple_of(2) {
            keys.push(element.clone());
        } else {
            values.push(element.clone());
            formats.push(format);
        }
    }

    let set_args = LoreBranchMetadataSetArgs {
        branch: resolve_branch_arg(&args.branch),
        keys: LoreArray::from_vec(keys),
        values: LoreArray::from_vec(values),
        formats: LoreArray::from_vec(formats),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                super::file::print_metadata(data, None, None);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(branch::metadata_set(globals, set_args, callback)) as u8
}

pub fn handle_branch_metadata_clear(globals: LoreGlobalArgs, args: &BranchMetadataClearArgs) -> u8 {
    let keys: Vec<LoreString> = args
        .keys
        .as_ref()
        .map(|k| k.iter().map(|s| LoreString::from(s.as_str())).collect())
        .unwrap_or_default();

    let clear_args = LoreBranchMetadataClearArgs {
        branch: resolve_branch_arg(&args.branch),
        keys: LoreArray::from_vec(keys),
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

    runtime().block_on(branch::metadata_clear(globals, clear_args, callback)) as u8
}

pub fn handle_branch_metadata_commands(
    cmd: &BranchMetadataCommands,
    globals: LoreGlobalArgs,
) -> u8 {
    match cmd {
        BranchMetadataCommands::Get(args) => handle_branch_metadata_get(globals, args),
        BranchMetadataCommands::Set(args) => handle_branch_metadata_set(globals, args),
        BranchMetadataCommands::Clear(args) => handle_branch_metadata_clear(globals, args),
    }
}

pub fn handle_branch_commands(cmd: &BranchCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        BranchCommands::Create(args) => {
            return handle_branch_create(globals, args);
        }
        BranchCommands::Info(args) => {
            return handle_branch_info(globals, args);
        }
        BranchCommands::Switch(args) => {
            return handle_branch_switch(globals, args);
        }
        BranchCommands::Push(args) => {
            return handle_branch_push(globals, args);
        }
        BranchCommands::Merge(args) => {
            return handle_branch_merge(globals, args);
        }
        BranchCommands::List(args) => {
            return handle_branch_list(globals, args);
        }
        BranchCommands::Diff(args) => {
            return handle_branch_diff(globals, args);
        }
        BranchCommands::Protect(args) => {
            return handle_branch_protect(globals, args);
        }
        BranchCommands::Unprotect(args) => {
            return handle_branch_unprotect(globals, args);
        }
        BranchCommands::Archive(args) => handle_branch_archive(globals, args),
        BranchCommands::Reset(args) => {
            return handle_branch_reset(globals, args);
        }
        BranchCommands::Latest(args) => {
            return handle_branch_latest_commands(globals, args);
        }
        BranchCommands::Metadata(args) => {
            return handle_branch_metadata_commands(&args.command, globals);
        }
    }
}
