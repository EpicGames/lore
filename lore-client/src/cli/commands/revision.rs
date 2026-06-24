// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clap::Args;
use clap::Subcommand;
use lore::auth;
use lore::auth::LoreAuthUserInfoArgs;
use lore::branch;
use lore::interface::Context;
use lore::interface::FragmentFlags;
use lore::interface::Hash;
use lore::interface::LoreArray;
use lore::interface::LoreBranchInfoArgs;
use lore::interface::LoreEvent;
use lore::interface::LoreEventCallback;
use lore::interface::LoreFragmentWriteEventData;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreMetadata;
use lore::interface::LoreMetadataEventData;
use lore::interface::LoreMetadataType;
use lore::interface::LoreRevisionAmendArgs;
use lore::interface::LoreRevisionBisectEventData;
use lore::interface::LoreRevisionCommitArgs;
use lore::interface::LoreRevisionCommitRevisionEventData;
use lore::interface::LoreRevisionDiffArgs;
use lore::interface::LoreRevisionHistoryArgs;
use lore::interface::LoreRevisionHistoryEntryEventData;
use lore::interface::LoreRevisionInfoArgs;
use lore::interface::LoreRevisionInfoDeltaEventData;
use lore::interface::LoreRevisionInfoEventData;
use lore::interface::LoreRevisionMetadataClearArgs;
use lore::interface::LoreRevisionMetadataGetArgs;
use lore::interface::LoreRevisionMetadataListArgs;
use lore::interface::LoreRevisionMetadataSetArgs;
use lore::interface::LoreRevisionRestoreArgs;
use lore::interface::LoreRevisionSyncArgs;
use lore::interface::LoreRevisionSyncFileEventData;
use lore::interface::LoreRevisionSyncRevisionEventData;
use lore::interface::LoreString;
use lore::interface::metadata;
use lore::revision;
use lore::revision::LoreRevisionBisectArgs;
use lore::revision::LoreRevisionFindArgs;
use lore::runtime;
use parking_lot::Mutex;

use super::file::print_metadata;
use super::file::user_ids_from_metadata;
use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::config::config;
use crate::pager::Pager;
use crate::print;
use crate::println;
use crate::progress_bar::ProgressBar;
use crate::progress_bar::progress_debug;
use crate::progress_bar::sync::apply_sync_progress_to_bar;
use crate::styling::BisectStyles;
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
pub struct RevisionArgs {
    /// Revision subcommand
    #[command(subcommand)]
    pub command: RevisionCommands,
}

#[derive(Args)]
pub struct RevisionMetadataArgs {
    #[command(subcommand)]
    pub command: RevisionMetadataCommands,
}

#[derive(Args)]
pub struct RevisionMetadataClearArgs {}

#[derive(Args)]
pub struct RevisionMetadataGetArgs {
    /// Attribute to get metadata for
    #[clap(value_name = "key")]
    key: Option<String>,
    /// Revision to get metadata for
    #[clap(long, value_name = "revision")]
    revision: Option<String>,
}

#[derive(Args)]
pub struct RevisionMetadataSetArgs {
    /// Metadata key/value pairs
    #[clap(value_name = "pairs", num_args = 1..)]
    pairs: Option<Vec<String>>,

    /// Indicator that values are paths to files
    #[clap(long, action)]
    binary: bool,
}

#[derive(Subcommand)]
pub enum RevisionMetadataCommands {
    /// Clear metadata for a staged revision
    Clear(RevisionMetadataClearArgs),

    /// Get metadata from a revision
    Get(RevisionMetadataGetArgs),

    /// Set metadata on for a staged revision
    Set(RevisionMetadataSetArgs),
}

#[derive(Args)]
pub struct RevisionHistoryArgs {
    /// Start listing from the specified revision. If not specified, start listing from the current branch latest revision
    #[clap(long, value_name = "revision", conflicts_with = "branch")]
    revision: Option<String>,

    /// Show branch revisions
    #[clap(long, value_name = "branch")]
    branch: Option<String>,

    /// Stop when reaching a revision created before this date (Unix timestamp)
    #[clap(long, value_name = "date", hide = true)]
    date: Option<u64>,

    /// Number of revisions to show
    pub length: Option<u32>,

    /// Stop when reaching a revision on a different branch (includes the branch point revision)
    #[clap(long, action)]
    pub only_branch: bool,

    /// Output each revision on one line only
    #[clap(long, action)]
    pub oneline: bool,
}

#[derive(Args)]
pub struct RevisionInfoArgs {
    /// Revision to get info for
    #[clap(value_name = "revision")]
    revision: Option<String>,
    /// Show delta information
    #[clap(long, action)]
    delta: bool,
    /// Show file metadata information
    #[clap(long, action)]
    metadata: bool,
}

#[derive(Args)]
pub struct RevisionCommitArgs {
    /// Commit message
    pub message: String,
    /// Print stats
    #[clap(long, action)]
    pub stats: bool,
    /// Commit only changes in this linked repository (mount path relative to repo root)
    #[clap(long, conflicts_with = "layer")]
    pub link: Option<String>,
    /// Per-link commit message. Takes two values: <path> <message>. Can be specified multiple times.
    #[clap(long = "link-message", num_args = 2, value_names = ["PATH", "MESSAGE"])]
    pub link_messages: Vec<String>,
    /// Commit only changes in this layer (mount path relative to repo root)
    #[clap(long)]
    pub layer: Option<String>,
    /// Per-layer commit message. Takes two values: <path> <message>. Can be specified multiple times.
    #[clap(long = "layer-message", num_args = 2, value_names = ["PATH", "MESSAGE"])]
    pub layer_messages: Vec<String>,
}

#[derive(Args)]
pub struct RevisionAmendArgs {
    /// Commit message
    pub message: String,
    /// Print stats
    #[clap(long, action)]
    pub stats: bool,
}

#[derive(Args)]
pub struct RevisionSyncArgs {
    /// Revision hash signature to synchronize to. Can be a signature on any
    /// branch — if the target revision is on a different branch, the current
    /// branch is updated accordingly. Can be a partial hash signature.
    #[clap(value_name = "revision")]
    revision: Option<String>,

    /// Fast forward any local changes if syncing to a local revision
    #[clap(long, action)]
    forward_changes: bool,

    /// Reset any local modified files to match the incoming revision
    #[clap(long, action, conflicts_with = "forward_changes")]
    reset: bool,

    /// Root files for dependency-based selective sync (only sync changes for these files and their dependencies)
    #[clap(long = "root-file", value_name = "path")]
    root_files: Vec<String>,

    /// Tags to filter dependencies by during dependency-based sync
    #[clap(long = "dependency-tag", value_name = "tag")]
    dependency_tags: Vec<String>,

    /// Follow transitive dependencies recursively during dependency-based sync
    #[clap(long, action)]
    dependency_recursive: bool,

    /// Maximum dependency traversal depth (0 means unlimited)
    #[clap(long, value_name = "depth", default_value = "0")]
    dependency_depth_limit: u32,
}

#[derive(Args)]
pub struct RevisionBisectArgs {
    /// The latest revision known to not have the change.
    #[clap(long, value_name = "start_revision")]
    pub start: String,
    /// The earliest revision known to have the change.
    #[clap(long, value_name = "end_revision")]
    pub end: String,
}

#[derive(Args)]
pub struct RevisionDiffArgs {
    /// Source revision to compare
    #[clap(value_name = "revision_source")]
    source: String,

    /// Target revision to compare, by default the current revision
    #[clap(long, value_name = "revision_target")]
    target: Option<String>,

    /// Optional path in repository
    #[clap(long)]
    path: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Subcommand)]
pub enum RevisionFindSubcommand {
    /// Find revision by metadata
    Metadata(RevisionFindMetadataArgs),

    /// Find revision by number
    Number(RevisionFindNumberArgs),
}

#[derive(Args)]
pub struct RevisionFindMetadataArgs {
    /// Metadata key to search for
    #[clap(value_name = "key")]
    key: String,

    /// Metadata value to match with
    #[clap(value_name = "value")]
    value: Option<String>,
}

#[derive(Args)]
pub struct RevisionFindNumberArgs {
    /// Revision number to search for
    number: u64,
}

#[derive(Args)]
pub struct RevisionFindArgs {
    #[command(subcommand)]
    subcommand: RevisionFindSubcommand,
}

#[derive(Args)]
pub struct RevisionRestoreArgs {
    /// Commit message
    pub message: Option<String>,
}

#[derive(Args)]
#[command(subcommand_negates_reqs = true)]
pub struct RevisionCherryPickArgs {
    /// Cherry-pick action
    #[command(subcommand)]
    subcommand: Option<RevisionCherryPickCommands>,

    /// Target revision to cherry-pick
    #[clap(value_name = "revision", required = true)]
    revision: Option<String>,

    /// Change the message for committing when no conflicts arise from the cherry-pick
    #[clap(long, action)]
    message: Option<String>,

    /// Disable auto commits even if no conflicts arise from the cherry-pick.
    #[clap(long, action)]
    no_commit: bool,
}

#[derive(Subcommand)]
pub enum RevisionCherryPickCommands {
    /// Marks the cherry-pick unresolved
    Unresolve(RevisionCherryPickTargetArgs),

    /// Restart the cherry-pick, resetting the current cherry-pick state
    Restart(RevisionCherryPickRestartArgs),

    /// Resolve conflicts
    Resolve(RevisionCherryPickResolveArgs),

    /// Abort a cherry-pick
    Abort,
}

#[derive(Args)]
pub struct RevisionCherryPickRestartArgs {
    #[clap(flatten)]
    targets: RevisionCherryPickTargetArgs,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct RevisionCherryPickResolveArgs {
    /// Resolve action
    #[command(subcommand)]
    subcommand: Option<RevisionCherryPickResolveCommands>,

    #[clap(flatten)]
    targets: RevisionCherryPickTargetArgs,
}

#[derive(Subcommand)]
pub enum RevisionCherryPickResolveCommands {
    /// Resolve using my changes
    Mine(RevisionCherryPickTargetArgs),

    /// Resolve using the incoming changes
    Theirs(RevisionCherryPickTargetArgs),
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct RevisionCherryPickTargetArgs {
    /// Any number of paths or files to target
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
#[command(subcommand_negates_reqs = true)]
pub struct RevisionRevertArgs {
    /// Revert action
    #[command(subcommand)]
    pub subcommand: Option<RevisionRevertCommands>,

    /// Target revision to revert
    #[clap(value_name = "revision", required = true)]
    pub revision: Option<String>,

    /// Change the message for committing when no conflicts arise from the revert
    #[clap(long, action)]
    pub message: Option<String>,

    /// Disable auto commits even if no conflicts arise from the revert.
    #[clap(long, action)]
    pub no_commit: bool,
}

#[derive(Subcommand)]
pub enum RevisionRevertCommands {
    /// Marks the revert unresolved
    Unresolve(RevisionRevertTargetArgs),

    /// Restart the revert, resetting the current revert state
    Restart(RevisionRevertRestartArgs),

    /// Resolve conflicts
    Resolve(RevisionRevertResolveArgs),

    /// Abort a revert
    Abort,
}

#[derive(Args)]
pub struct RevisionRevertRestartArgs {
    #[clap(flatten)]
    pub targets: RevisionRevertTargetArgs,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct RevisionRevertResolveArgs {
    /// Resolve action
    #[command(subcommand)]
    pub subcommand: Option<RevisionRevertResolveCommands>,

    #[clap(flatten)]
    pub targets: RevisionRevertTargetArgs,
}

#[derive(Subcommand)]
pub enum RevisionRevertResolveCommands {
    /// Resolve using my changes
    Mine(RevisionRevertTargetArgs),

    /// Resolve using the incoming changes
    Theirs(RevisionRevertTargetArgs),
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct RevisionRevertTargetArgs {
    /// Any number of paths or files to target
    #[clap(value_name = "paths", num_args = 1..)]
    pub paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    pub targets: Option<String>,
}

#[derive(Subcommand)]
pub enum RevisionCommands {
    /// List revisions of a repository
    History(RevisionHistoryArgs),

    /// Get info about a revision
    Info(RevisionInfoArgs),

    /// Commit the staged state
    Commit(RevisionCommitArgs),

    /// Amend the latest commit's message
    Amend(RevisionAmendArgs),

    /// Synchronize to a given state of a repository
    #[clap(visible_alias("synchronize"))]
    Sync(RevisionSyncArgs),

    /// Binary search for a change introduced between start (exclusive) and end (inclusive.)
    Bisect(RevisionBisectArgs),

    /// Diff two revisions
    Diff(RevisionDiffArgs),

    /// Find revision
    Find(RevisionFindArgs),

    /// Restore current revision as latest revision
    Restore(RevisionRestoreArgs),

    /// Cherry-pick a revision onto the currently synced revision
    #[clap(name = "cherry-pick")]
    CherryPick(RevisionCherryPickArgs),

    /// Revert a revision from the currently synced revision
    Revert(RevisionRevertArgs),

    // File,
    /// Manage metadata of a given revision
    Metadata(RevisionMetadataArgs),
}

struct RevisionEntryDelta {
    action: String,
    path: String,
    merged: String,
    metadata: Option<Vec<LoreMetadataEventData>>,
}

struct RevisionEntryData {
    repository: Context,
    revision_number: u64,
    signature: Hash,
    parent: Option<Hash>,
    merge: Option<Hash>,
    metadata: Option<Vec<LoreMetadataEventData>>,
    delta: Option<Vec<RevisionEntryDelta>>,
}

impl RevisionEntryData {
    fn message(&self) -> Option<&str> {
        self.metadata.as_ref()?.iter().find_map(|meta| {
            if meta.key.as_str() != metadata::MESSAGE {
                return None;
            }
            if let LoreMetadata::String(value) = &meta.value {
                Some(value.as_str())
            } else {
                None
            }
        })
    }

    fn is_merge(&self) -> bool {
        self.parent.is_some() && self.merge.is_some()
    }

    fn print_description(&self, auth_data: Option<&HashMap<String, String>>) {
        println!(
            "{}Repository: {}{}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            self.repository
        );
        println!(
            "{}Revision  : {}{}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            self.revision_number
        );
        println!(
            "{}Signature : {}{}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            self.signature
        );

        if let Some(parent) = self.parent {
            println!(
                "{}Parent    : {}{parent}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
        if let Some(merge) = self.merge {
            println!(
                "{}Parent    : {}{merge}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
        if let Some(metadata) = self.metadata.as_ref() {
            for metadata in metadata.iter() {
                print_metadata(metadata, auth_data, None);
            }
        }
    }
}

pub fn handle_revision_history(globals: LoreGlobalArgs, args: &RevisionHistoryArgs) -> u8 {
    let list_args = LoreRevisionHistoryArgs {
        revision: LoreString::from(&args.revision),
        branch: LoreString::from(&args.branch),
        date: args.date.unwrap_or_default(),
        length: args.length.unwrap_or_default(),
        only_branch: args.only_branch as u8,
    };

    let list_entry_data: Arc<Mutex<Vec<RevisionEntryData>>> = Arc::new(Mutex::new(Vec::default()));

    let list_entry_data_clone = list_entry_data.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionHistoryEntry(data) => {
                store_list_data(data, list_entry_data_clone.clone());
            }
            LoreEvent::Metadata(data) => store_metadata(data, list_entry_data_clone.clone()),
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let list_result =
        runtime().block_on(revision::history(globals.clone(), list_args, callback)) as u8;

    // If the revision list returned an error then don't bother resolving usernames
    if list_result != 0 {
        return list_result;
    }

    let auth_data = resolve_revision_user_ids(globals.clone(), list_entry_data.clone());

    let _pager = Pager::new();

    let list_entry_data = list_entry_data.lock();
    let mut revision_to_branch: HashMap<Hash, Option<String>> = HashMap::new();
    let mut branch_to_name: HashMap<Context, Option<String>> = HashMap::new();
    for entry in list_entry_data.iter() {
        if args.oneline {
            let revision_number = entry.revision_number;

            let message = entry.message().unwrap_or_default();
            let message = if message.is_empty() && entry.is_merge() {
                auto_merge_message(
                    &globals,
                    entry,
                    &mut revision_to_branch,
                    &mut branch_to_name,
                )
                .unwrap_or_default()
            } else {
                message.lines().next().map(String::from).unwrap_or_default()
            };

            println!("{revision_number} {message}");
        } else {
            println!(
                "{}Revision  :{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                entry.revision_number
            );
            println!(
                "{}Signature :{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                entry.signature
            );
            if let Some(merge) = entry.merge {
                println!(
                    "{}Merge     :{} {merge}",
                    CommonStyles::HEADERS,
                    anstyle::Reset
                );
            }
            let synthetic_merge_message =
                if entry.is_merge() && entry.message().unwrap_or_default().is_empty() {
                    auto_merge_message(
                        &globals,
                        entry,
                        &mut revision_to_branch,
                        &mut branch_to_name,
                    )
                } else {
                    None
                };
            if let Some(metadata) = entry.metadata.as_ref() {
                for metadata in metadata.iter() {
                    print_metadata(
                        metadata,
                        Some(&auth_data),
                        synthetic_merge_message.as_deref(),
                    );
                }
            }

            println!();
        }
    }

    list_result
}

pub fn handle_revision_info(globals: LoreGlobalArgs, args: &RevisionInfoArgs) -> u8 {
    let info_args = LoreRevisionInfoArgs {
        revision: LoreString::from(&args.revision),
        delta: args.delta.into(),
        metadata: args.metadata.into(),
    };

    enum SearchHighlight {
        None,
        RevisionNumber,
        Revision { match_len: usize },
    }
    let search_highlight = {
        let revision = info_args.revision.as_str();
        if revision.is_empty() {
            SearchHighlight::None
        } else if revision.contains("@") {
            SearchHighlight::RevisionNumber
        } else {
            SearchHighlight::Revision {
                match_len: revision.len(),
            }
        }
    };

    let info_entry_data: Arc<Mutex<Vec<RevisionEntryData>>> = Arc::new(Mutex::new(Vec::default()));

    let info_entry_data_clone = info_entry_data.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionInfo(data) => {
                store_info_data(data, info_entry_data_clone.clone());
            }
            LoreEvent::Metadata(data) => {
                store_metadata(data, info_entry_data_clone.clone());
            }
            LoreEvent::RevisionInfoDelta(data) => {
                store_info_delta_data(data, info_entry_data_clone.clone());
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let info_result =
        runtime().block_on(revision::info(globals.clone(), info_args, callback)) as u8;

    // If the revision info returned an error then don't bother resolving usernames
    if info_result != 0 {
        return info_result;
    }

    let auth_data = resolve_revision_user_ids(globals, info_entry_data.clone());

    let info_entry_data = info_entry_data.lock();
    for entry in info_entry_data.iter() {
        println!(
            "{}Revision  :{} {}{}{}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            if matches!(search_highlight, SearchHighlight::RevisionNumber) {
                CommonStyles::SEARCH_HIGHLIGHT
            } else {
                CommonStyles::DEFAULT
            },
            entry.revision_number,
            anstyle::Reset
        );
        println!(
            "{}Signature :{} {}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            if let SearchHighlight::Revision { match_len } = search_highlight {
                let signature_string = entry.signature.to_string();
                let (left, right) = signature_string.split_at(match_len);
                format!(
                    "{}{}{}{}",
                    CommonStyles::SEARCH_HIGHLIGHT,
                    left,
                    anstyle::Reset,
                    right
                )
            } else {
                entry.signature.to_string()
            }
        );
        if let Some(parent) = entry.parent {
            if let Some(merge) = entry.merge {
                println!(
                    "{}Merge     :{} {parent} {merge}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                );
            } else {
                println!(
                    "{}Parent    :{} {parent}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                );
            }
        }
        if let Some(metadata) = entry.metadata.as_ref() {
            for metadata in metadata.iter() {
                print_metadata(metadata, Some(&auth_data), None);
            }
        }
        println!();
        if let Some(delta) = entry.delta.as_ref() {
            for delta in delta.iter() {
                let action_style = match delta.action.chars().next() {
                    Some('A') => FileActionStyle::ADDED,
                    Some('D') => FileActionStyle::DELETED,
                    _ => FileActionStyle::MODIFIED,
                };
                println!(
                    "{}{}{} {} {}",
                    action_style,
                    delta.action,
                    anstyle::Reset,
                    delta.path,
                    delta.merged
                );
                if let Some(metadata) = delta.metadata.as_ref() {
                    for metadata in metadata.iter() {
                        print_metadata(metadata, Some(&auth_data), None);
                    }
                }
            }
        }
    }

    info_result
}

const STATS_SIZE_BUCKETS: usize = 64;

#[derive(Default, Clone)]
struct FragmentStats {
    pub total_state_count: usize,
    pub total_fragmentlist_count: usize,
    pub total_written_count: usize,
    pub total_written_raw: usize,
    pub total_written_payload: usize,
    pub total_dedup_count: usize,
    pub total_dedup_raw: usize,
    pub total_dedup_payload: usize,
    pub size_count: Vec<usize>,
}

impl FragmentStats {
    fn complete(&self) {
        let total_written_count = self.total_written_count;
        let total_written_raw = self.total_written_raw;
        let total_written_payload = self.total_written_payload;

        let total_dedup_count = self.total_dedup_count;
        let total_dedup_raw = self.total_dedup_raw;
        let total_dedup_payload = self.total_dedup_payload;

        let total_fragmentlist_count = self.total_fragmentlist_count;
        let total_state_count = self.total_state_count;

        let compression_rate = if total_written_raw > 0 {
            1.0 - (total_written_payload as f64) / (total_written_raw as f64)
        } else {
            0.0
        };
        let compression_percentage = (compression_rate * 100.0) as u32;

        let dedup_rate = if total_written_count > 0 {
            (total_dedup_count as f64) / (total_written_count as f64)
        } else {
            0.0
        };
        let dedup_count_percentage = (dedup_rate * 100.0) as u32;

        let dedup_rate = if total_written_raw > 0 {
            (total_dedup_raw as f64) / (total_written_raw as f64)
        } else {
            0.0
        };
        let dedup_raw_percentage = (dedup_rate * 100.0) as u32;

        let dedup_rate = if total_written_payload > 0 {
            (total_dedup_payload as f64) / (total_written_payload as f64)
        } else {
            0.0
        };
        let dedup_payload_percentage = (dedup_rate * 100.0) as u32;

        let final_bytes = total_written_payload - total_dedup_payload;
        let final_rate = if total_written_raw > 0 {
            (final_bytes as f64) / (total_written_raw as f64)
        } else {
            0.0
        };
        let final_percentage = (final_rate * 100.0) as u32;

        println!("Commit self");
        println!("  Written fragments         : {total_written_count}");
        println!("  Written raw bytes         : {total_written_raw}");
        println!(
            "  Written payload bytes     : {total_written_payload} ({compression_percentage}% compression)"
        );
        println!("  Deduplicated fragments    : {total_dedup_count} ({dedup_count_percentage}%)");
        println!("  Deduplicated raw bytes    : {total_dedup_raw} ({dedup_raw_percentage}%)");
        println!(
            "  Deduplicated payload bytes: {total_dedup_payload} ({dedup_payload_percentage}%)"
        );
        println!("  Written final bytes:      : {final_bytes} ({final_percentage}%)");
        println!("  Written list fragments    : {total_fragmentlist_count}");
        println!("  Written state fragments   : {total_state_count}");

        println!("Chunk size distribution:");
        let bucket_count = self.size_count.len();
        let max_count = self.size_count.iter().max().cloned().unwrap_or_default();
        for (bucket, count) in self.size_count.iter().enumerate() {
            let start_size = (((bucket as f64) / (bucket_count as f64))
                * (lore_base::types::FRAGMENT_SIZE_THRESHOLD as f64))
                as usize;
            let end_size = ((((bucket + 1) as f64) / (bucket_count as f64))
                * (lore_base::types::FRAGMENT_SIZE_THRESHOLD as f64))
                as usize;

            let count_len = if max_count == 0 {
                0
            } else {
                let count_frac = ((1 + count) as f64) / (max_count as f64);
                (40.0 * count_frac).clamp(0.0, 40.0) as usize
            };
            let count_percent = if total_written_count == 0 {
                0.0
            } else {
                100.0 * (*count as f64) / (total_written_count as f64)
            };
            let stars = "*".repeat(count_len);
            println!(
                "{start_size:>6} - {end_size:>6}: {stars:<40} ({count:<6}) {count_percent:.2}%"
            );
        }
    }

    fn add_fragment_write(&mut self, data: &LoreFragmentWriteEventData) {
        if (data.fragment.flags & FragmentFlags::PayloadFragmented) != 0 {
            // Fragment list
            self.total_fragmentlist_count += 1;
        } else if (data.fragment.flags & FragmentFlags::PayloadRevisionState) != 0 {
            // State data
            self.total_state_count += 1;
        } else {
            // File data
            if data.deduplicated > 0 {
                self.total_dedup_count += 1;
                self.total_dedup_raw += data.fragment.size_content as usize;
                self.total_dedup_payload += data.fragment.size_payload as usize;
            }
            self.total_written_count += 1;
            self.total_written_raw += data.fragment.size_content as usize;
            self.total_written_payload += data.fragment.size_payload as usize;

            let size_bucket = (data.fragment.size_content as f64)
                / (lore_base::types::FRAGMENT_SIZE_THRESHOLD as f64);
            let size_bucket = std::cmp::min(
                (size_bucket * (self.size_count.len() as f64)) as usize,
                self.size_count.len() - 1,
            );
            self.size_count[size_bucket] += 1;
        }
    }
}

fn resolve_link_messages(
    globals: &LoreGlobalArgs,
    args: &RevisionCommitArgs,
) -> Result<(Vec<LoreString>, Vec<LoreString>), u8> {
    let mut link_paths: Vec<LoreString> = Vec::new();
    let mut link_msgs: Vec<LoreString> = Vec::new();

    let link_message_values = &args.link_messages;
    for pair in link_message_values.chunks(2) {
        if pair.len() == 2 {
            link_paths.push(LoreString::from(pair[0].as_str()));
            link_msgs.push(LoreString::from(pair[1].as_str()));
        }
    }

    let is_interactive = !config().non_interactive && !config().json;
    let has_explicit_flags = !link_message_values.is_empty();
    let is_link_scoped = args.link.is_some();

    if is_interactive && !has_explicit_flags && !is_link_scoped {
        let discovered_links: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::default()));
        let discovered_links_clone = discovered_links.clone();
        let discovery_callback: LoreEventCallback = Some(
            (Box::new(move |event: &LoreEvent| {
                if let LoreEvent::LinkStagedEntry(data) = event {
                    // Pin-only updates (no staged files in the linked repo) fall through to the
                    // standard non-interactive commit — nothing worth a per-link message.
                    if data.staged_file_count == 0 {
                        return;
                    }
                    discovered_links_clone
                        .lock()
                        .push((data.path.to_string(), data.staged_file_count));
                }
            }) as EventCallbackFn)
                .with_defaults(),
        );

        runtime().block_on(lore::link::list_staged(globals.clone(), discovery_callback));

        let links = discovered_links.lock().clone();
        if !links.is_empty() {
            // Hold a progress bar suspend token for the entire interactive section
            // to prevent the progress bar from rendering escape sequences that
            // corrupt the terminal line editing during stdin read_line.
            let _suspend = crate::progress_bar::suspend_current_progress_bar();

            println!(
                "\n  {}Linked repositories with staged changes:{}",
                CommonStyles::HEADERS,
                anstyle::Reset,
            );
            for (path, file_count) in &links {
                println!(
                    "    {}{}{} ({} file{} changed)",
                    CommonStyles::SUCCESS,
                    path,
                    anstyle::Reset,
                    file_count,
                    if *file_count == 1 { "" } else { "s" }
                );
            }
            println!("\n  Press Enter to use the main commit message.\n");

            for (path, file_count) in &links {
                println!(
                    "  Commit message for link {}{}{} ({} file{} changed):",
                    CommonStyles::SUCCESS,
                    path,
                    anstyle::Reset,
                    file_count,
                    if *file_count == 1 { "" } else { "s" }
                );
                let _ = std::io::Write::write_all(&mut std::io::stdout(), b"  > ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut input = String::new();
                match crate::util::read_line_with_editing(&mut input) {
                    Ok(_) => {
                        let input = input.trim();
                        if input.is_empty() {
                            println!("  Using main message.");
                        } else {
                            link_paths.push(LoreString::from(path.as_str()));
                            link_msgs.push(LoreString::from(input));
                        }
                    }
                    Err(err) => {
                        println!(
                            "  {}Warning: failed to read input for link '{}': {}. Using main message.{}",
                            CommonStyles::MAINTENANCE,
                            path,
                            err,
                            anstyle::Reset
                        );
                    }
                }
            }
            println!();
        }
    }

    if !link_paths.is_empty() {
        let discovered_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::default()));
        let discovered_paths_clone = discovered_paths.clone();
        let validation_callback: LoreEventCallback = Some(
            (Box::new(move |event: &LoreEvent| {
                if let LoreEvent::LinkStagedEntry(data) = event {
                    discovered_paths_clone.lock().push(data.path.to_string());
                }
            }) as EventCallbackFn)
                .with_defaults(),
        );

        runtime().block_on(lore::link::list_staged(
            globals.clone(),
            validation_callback,
        ));

        let valid_paths = discovered_paths.lock().clone();
        for path in link_paths.iter() {
            if !valid_paths.iter().any(|p| p == path.as_str()) {
                println!(
                    "{}Error: --link-message path '{}' does not match any linked repository with staged changes{}",
                    CommonStyles::FAILURE,
                    path.as_str(),
                    anstyle::Reset
                );
                return Err(1);
            }
        }
    }

    Ok((link_paths, link_msgs))
}

fn resolve_layer_messages(
    globals: &LoreGlobalArgs,
    args: &RevisionCommitArgs,
) -> Result<(Vec<LoreString>, Vec<LoreString>), u8> {
    let mut layer_paths: Vec<LoreString> = Vec::new();
    let mut layer_msgs: Vec<LoreString> = Vec::new();

    for pair in args.layer_messages.chunks(2) {
        if pair.len() == 2 {
            layer_paths.push(LoreString::from(pair[0].as_str()));
            layer_msgs.push(LoreString::from(pair[1].as_str()));
        }
    }

    let is_interactive = !config().non_interactive && !config().json;
    let has_explicit_flags = !args.layer_messages.is_empty();

    if is_interactive && !has_explicit_flags {
        let discovered_layers: Arc<Mutex<Vec<(String, u64)>>> =
            Arc::new(Mutex::new(Vec::default()));
        let discovered_layers_clone = discovered_layers.clone();
        let discovery_callback: LoreEventCallback = Some(
            (Box::new(move |event: &LoreEvent| {
                if let LoreEvent::LayerStagedEntry(data) = event {
                    if data.staged_file_count == 0 {
                        return;
                    }
                    discovered_layers_clone
                        .lock()
                        .push((data.target_path.to_string(), data.staged_file_count));
                }
            }) as EventCallbackFn)
                .with_defaults(),
        );

        runtime().block_on(lore::layer::layer_list_staged(
            globals.clone(),
            lore::layer::LoreLayerListStagedArgs {},
            discovery_callback,
        ));

        let layers = discovered_layers.lock().clone();
        if !layers.is_empty() {
            // Hold a progress bar suspend token for the entire interactive
            // section to prevent the progress bar from rendering escape
            // sequences that corrupt terminal line editing during stdin reads.
            let _suspend = crate::progress_bar::suspend_current_progress_bar();

            println!(
                "\n  {}Layers with staged changes:{}",
                CommonStyles::HEADERS,
                anstyle::Reset,
            );
            for (path, file_count) in &layers {
                println!(
                    "    {}{}{} ({} file{} changed)",
                    CommonStyles::SUCCESS,
                    path,
                    anstyle::Reset,
                    file_count,
                    if *file_count == 1 { "" } else { "s" }
                );
            }
            println!("\n  Press Enter to use the main commit message.\n");

            for (path, file_count) in &layers {
                println!(
                    "  Commit message for layer {}{}{} ({} file{} changed):",
                    CommonStyles::SUCCESS,
                    path,
                    anstyle::Reset,
                    file_count,
                    if *file_count == 1 { "" } else { "s" }
                );
                let _ = std::io::Write::write_all(&mut std::io::stdout(), b"  > ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut input = String::new();
                match crate::util::read_line_with_editing(&mut input) {
                    Ok(_) => {
                        let input = input.trim();
                        if input.is_empty() {
                            println!("  Using main message.");
                        } else {
                            layer_paths.push(LoreString::from(path.as_str()));
                            layer_msgs.push(LoreString::from(input));
                        }
                    }
                    Err(err) => {
                        println!(
                            "  {}Warning: failed to read input for layer '{}': {}. Using main message.{}",
                            CommonStyles::MAINTENANCE,
                            path,
                            err,
                            anstyle::Reset
                        );
                    }
                }
            }
            println!();
        }
    }

    if !layer_paths.is_empty() {
        // Validate each --layer-message path against the configured layers.
        let configured_layers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::default()));
        let configured_layers_clone = configured_layers.clone();
        let validation_callback: LoreEventCallback = Some(
            (Box::new(move |event: &LoreEvent| {
                if let LoreEvent::LayerEntry(data) = event {
                    configured_layers_clone
                        .lock()
                        .push(data.target_path.to_string());
                }
            }) as EventCallbackFn)
                .with_defaults(),
        );

        runtime().block_on(lore::layer::layer_list(
            globals.clone(),
            lore::layer::LoreLayerListArgs {},
            validation_callback,
        ));

        let valid_paths = configured_layers.lock().clone();
        for path in layer_paths.iter() {
            if !valid_paths.iter().any(|p| p == path.as_str()) {
                println!(
                    "{}Error: --layer-message path '{}' does not match any configured layer{}",
                    CommonStyles::FAILURE,
                    path.as_str(),
                    anstyle::Reset
                );
                return Err(1);
            }
        }
    }

    Ok((layer_paths, layer_msgs))
}

pub fn handle_revision_commit(globals: LoreGlobalArgs, args: &RevisionCommitArgs) -> u8 {
    let dry_run = globals.dry_run();
    let mut fragment_stats = FragmentStats::default();
    fragment_stats.size_count.resize(STATS_SIZE_BUCKETS, 0);

    let fragment_stats = Arc::new(Mutex::new(fragment_stats));
    let print_stats = args.stats;

    let (link_paths, link_msgs) = match resolve_link_messages(&globals, args) {
        Ok(result) => result,
        Err(code) => return code,
    };

    let (layer_paths, layer_msgs) = match resolve_layer_messages(&globals, args) {
        Ok(result) => result,
        Err(code) => return code,
    };

    let commit_args = LoreRevisionCommitArgs {
        message: LoreString::from(&args.message),
        link: LoreString::from(args.link.as_deref().unwrap_or("")),
        link_paths: LoreArray::from_vec(link_paths),
        link_messages: LoreArray::from_vec(link_msgs),
        layer: LoreString::from(args.layer.as_deref().unwrap_or("")),
        layer_paths: LoreArray::from_vec(layer_paths),
        layer_messages: LoreArray::from_vec(layer_msgs),
        stats: args.stats,
    };

    let commit_entry_data: Arc<Mutex<Vec<RevisionEntryData>>> =
        Arc::new(Mutex::new(Vec::default()));

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let commit_entry_data_clone = commit_entry_data.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionCommitBegin(_data) => {
                if dry_run {
                    println!("Previewing commit of staged changes");
                } else {
                    println!("Committing staged changes");
                }
            }
            LoreEvent::RevisionCommitProgress(data) => {
                let estimate = if data.count.discovery_complete != 0 {
                    ""
                } else {
                    "+"
                };
                let bytes_info = if data.count.bytes_total > 0 {
                    format!(
                        ", {}/{}{}",
                        format_bytes_to_string(data.count.bytes_transferred),
                        format_bytes_to_string(data.count.bytes_total),
                        estimate,
                    )
                } else {
                    String::new()
                };
                if debug {
                    print!(
                        "[Debug] Committing {}/{} directories, {}/{}{} files{} ({} modified, {} deleted)",
                        data.count.directory_count,
                        data.count.directory_total,
                        data.count.file_count,
                        data.count.file_total,
                        estimate,
                        bytes_info,
                        data.count.file_modify_count,
                        data.count.file_delete_count,
                    );
                }
                progress_bar.set_max_progress(data.count.directory_total + data.count.file_total);
                progress_bar.set_progress(data.count.directory_count + data.count.file_count);
                progress_bar.set_growing(data.count.discovery_complete == 0);
            }
            LoreEvent::RevisionCommitEnd(data)
                if (data.count.file_count > 0 || data.count.directory_count > 0) => {
                    let bytes_info = if data.count.bytes_total > 0 {
                        format!(
                            ", {}/{}",
                            format_bytes_to_string(data.count.bytes_transferred),
                            format_bytes_to_string(data.count.bytes_total),
                        )
                    } else {
                        String::new()
                    };
                    println!(
                        "{} {}/{} directories, {}/{} files{} ({} modified, {} deleted)",
                        if dry_run { "Would commit" } else { "Committed" },
                        data.count.directory_count,
                        data.count.directory_total,
                        data.count.file_count,
                        data.count.file_total,
                        bytes_info,
                        data.count.file_modify_count,
                        data.count.file_delete_count,
                    );
                }
            LoreEvent::RevisionCommitRevision(data) => {
                store_commit_data(data, commit_entry_data_clone.clone());
            }
            LoreEvent::Metadata(data) => store_metadata(data, commit_entry_data_clone.clone()),
            LoreEvent::Complete(data)
                if data.status == 0 && print_stats => {
                    let stats = fragment_stats.lock();
                    stats.complete();
                }
            LoreEvent::FragmentWrite(data)
                if print_stats => {
                    let mut stats = fragment_stats.lock();
                    stats.add_fragment_write(data);
                }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let commit_result =
        runtime().block_on(revision::commit(globals.clone(), commit_args, callback)) as u8;

    // If the revision commit returned an error then don't bother resolving usernames
    if commit_result != 0 {
        println!("{}Commit failed{}", CommonStyles::FAILURE, anstyle::Reset);
        return commit_result;
    }

    let auth_data = resolve_revision_user_ids(globals, commit_entry_data.clone());

    let describe_entry_data = commit_entry_data.lock();
    for entry in describe_entry_data.iter() {
        entry.print_description(Some(&auth_data));
        println!(
            "{}{}{}",
            CommonStyles::SUCCESS,
            if dry_run {
                "Dry-run commit succeeded"
            } else {
                "Commit succeeded"
            },
            anstyle::Reset
        );
        println!();
    }

    commit_result
}

pub fn handle_revision_amend(globals: LoreGlobalArgs, args: &RevisionAmendArgs) -> u8 {
    let mut fragment_stats = FragmentStats::default();
    fragment_stats.size_count.resize(STATS_SIZE_BUCKETS, 0);

    let fragment_stats = Arc::new(Mutex::new(fragment_stats));
    let print_stats = args.stats;

    let amend_args = LoreRevisionAmendArgs {
        message: LoreString::from(&args.message),
    };

    let amended_entry_data: Arc<Mutex<Vec<RevisionEntryData>>> =
        Arc::new(Mutex::new(Vec::default()));

    let amended_entry_data_clone = amended_entry_data.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionCommitRevision(data) => {
                store_commit_data(data, amended_entry_data_clone.clone());
            }
            LoreEvent::Metadata(data) => store_metadata(data, amended_entry_data_clone.clone()),
            LoreEvent::Complete(data) if data.status == 0 && print_stats => {
                let stats = fragment_stats.lock();
                stats.complete();
            }
            LoreEvent::FragmentWrite(data) if print_stats => {
                let mut stats = fragment_stats.lock();
                stats.add_fragment_write(data);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => {}
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let amend_result =
        runtime().block_on(revision::amend(globals.clone(), amend_args, callback)) as u8;

    // If the revision amend returned an error then don't bother resolving usernames
    if amend_result != 0 {
        println!("{}Amend failed{}", CommonStyles::FAILURE, anstyle::Reset);
        return amend_result;
    }

    let auth_data = resolve_revision_user_ids(globals, amended_entry_data.clone());

    let describe_entry_data = amended_entry_data.lock();
    for entry in describe_entry_data.iter() {
        entry.print_description(Some(&auth_data));
        println!("{}Amend succeeded{}", CommonStyles::SUCCESS, anstyle::Reset);
        println!();
    }

    amend_result
}

fn file_info_display(file: &LoreRevisionSyncFileEventData) -> String {
    format!(
        "Sync file {}, size {}, action {:?}, file flag {}",
        file.path, file.size, file.action, file.flag_file
    )
}

fn revision_info_display(revision: &LoreRevisionSyncRevisionEventData) -> String {
    format!(
        "Sync revision {}, number {}, merge flag {}, conflict flag {}",
        revision.revision, revision.revision_number, revision.flag_merge, revision.flag_conflict
    )
}

pub fn handle_sync_event(event: &LoreEvent, progress_bar: &ProgressBar, debug: bool) {
    match event {
        LoreEvent::RevisionSyncTarget(data) if data.source_revision == data.target_revision => {
            if data.is_latest != 0 {
                println!(
                    "Already on branch {} latest revision {} -> {}",
                    data.branch_name, data.target_revision_number, data.target_revision
                );
            } else {
                println!(
                    "Already on revision {} -> {}",
                    data.target_revision_number, data.target_revision
                );
            }
        }
        LoreEvent::RevisionSyncTarget(data) => {
            if !data.remote.is_empty() {
                println!("Sync from remote {}", data.remote);
            }
            println!(
                "On branch {} revision {} -> {}",
                data.branch_name, data.source_revision_number, data.source_revision
            );
            let local = if data.local != 0 { " local" } else { "" };
            println!(
                "Synchronizing to{} revision {} -> {}",
                local, data.target_revision_number, data.target_revision
            );
        }
        LoreEvent::RevisionSyncFile(data) if debug => {
            println!("[Debug] {}", file_info_display(data));
        }
        LoreEvent::RevisionSyncProgress(data) => {
            apply_sync_progress_to_bar(progress_bar, data);

            if debug {
                print!("[Debug] {}", progress_info_display(data));
            }
        }
        LoreEvent::RevisionSyncRevision(data) if debug => {
            println!("[Debug] {}", revision_info_display(data));
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
        LoreEvent::Complete(_) if !debug => {
            println!();
        }
        LoreEvent::Maintenance(data) => {
            util::handle_maintenance_event(data);
        }
        _ => (),
    }
}

pub fn handle_revision_sync(globals: LoreGlobalArgs, args: &RevisionSyncArgs) -> u8 {
    let debug = progress_debug();

    let sync_args = LoreRevisionSyncArgs {
        revision: LoreString::from(&args.revision),
        forward_changes: args.forward_changes.into(),
        reset: args.reset.into(),
        root_files: LoreArray::from_vec(
            args.root_files
                .iter()
                .map(|s| LoreString::from(s.as_str()))
                .collect(),
        ),
        dependency_tags: LoreArray::from_vec(
            args.dependency_tags
                .iter()
                .map(|s| LoreString::from(s.as_str()))
                .collect(),
        ),
        dependency_recursive: args.dependency_recursive.into(),
        dependency_depth_limit: args.dependency_depth_limit,
    };

    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| {
            handle_sync_event(event, &progress_bar, debug);
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(revision::sync(globals, sync_args, callback)) as u8;
}

pub fn handle_revision_bisect(globals: LoreGlobalArgs, args: &RevisionBisectArgs) -> u8 {
    let debug = progress_debug();

    let bisect_args = LoreRevisionBisectArgs {
        start: LoreString::from(&args.start),
        end: LoreString::from(&args.end),
    };

    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionBisect(bisect_data) => {
                                let LoreRevisionBisectEventData {
                    start_revision_number,
                    target_revision_number,
                    end_revision_number,
                    done,
                } = bisect_data;

                {
                    let rev_style = BisectStyles::REVISION;
                    let cmd_style = BisectStyles::COMMAND;
                    let complete_style = BisectStyles::SUCCESS;
                    let step_complete_style = BisectStyles::STEP_SUCCESS;
                    let bold_style = BisectStyles::EMPHASIS;
                    let reset_style = anstyle::Reset;

                    println!();
                    println!("Synchronized to {rev_style}@{target_revision_number}{reset_style}");
                    println!();
                    if *done > 0 {
                        println!("Revision {rev_style}@{target_revision_number}{reset_style} contains the change being searched for");
                        println!("{complete_style}Bisect complete{reset_style}");
                    } else {
                        let print_rev_cmd = |start: &u64, end: &u64| {
                            println!();
                            println!("\t{cmd_style}lore revision bisect --start {reset_style}{rev_style}@{start}{reset_style} {cmd_style}--end {reset_style}{rev_style}@{end}{reset_style}");
                            println!();
                        };
                        println!("If this revision {bold_style}does{reset_style} contain the change being searched for:");
                        print_rev_cmd(start_revision_number, target_revision_number);
                        println!("If this revision {bold_style}does not{reset_style} contain the change being searched for:");
                        print_rev_cmd(target_revision_number, end_revision_number);
                        println!("{step_complete_style}Bisect step complete{reset_style}");
                    }
                }
            }
            _ => handle_sync_event(event, &progress_bar, debug),
        }) as EventCallbackFn)
            .with_defaults(),
    ));
    runtime().block_on(revision::bisect(globals, bisect_args, callback)) as u8
}

pub fn handle_revision_diff(globals: LoreGlobalArgs, args: &RevisionDiffArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.path, &args.targets);

    let diff_args = LoreRevisionDiffArgs {
        revision_source: LoreString::from(&args.source),
        revision_target: LoreString::from(&args.target),
        paths,
    };

    let _pager = Pager::new();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionDiffFile(data) => {
                println!(
                    "{}{}{} {}",
                    FileActionStyle::from_action(data.action),
                    data.action_as_string_short(),
                    anstyle::Reset,
                    data.path.as_str()
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

    return runtime().block_on(revision::diff(globals, diff_args, callback)) as u8;
}

pub fn handle_revision_find(globals: LoreGlobalArgs, args: &RevisionFindArgs) -> u8 {
    let find_args = match &args.subcommand {
        RevisionFindSubcommand::Metadata(args) => LoreRevisionFindArgs {
            key: LoreString::from(&args.key),
            value: LoreString::from(&args.value),
            number: 0,
        },
        RevisionFindSubcommand::Number(args) => LoreRevisionFindArgs {
            key: LoreString::default(),
            value: LoreString::default(),
            number: args.number,
        },
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionFind(data) => {
                if data.signature.is_zero() {
                    println!("No matching revision found");
                } else {
                    println!("{}", data.signature);
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

    return runtime().block_on(revision::find(globals, find_args, callback)) as u8;
}

pub fn handle_revision_restore(globals: LoreGlobalArgs, args: &RevisionRestoreArgs) -> u8 {
    let restore_args = LoreRevisionRestoreArgs {
        message: LoreString::from(&args.message),
    };

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevisionRestoreFileBegin(data) => {
                progress_bar.set_max_progress(data.count as u64);
                println!(
                    "Found {} files and/or directories potentially affected by restore",
                    data.count
                );
            }
            LoreEvent::RevisionRestoreFile(data) => {
                println!("{}", data.path.as_str());
            }
            LoreEvent::RevisionRestoreFragmentBegin(data) if data.fragments > 0 => {
                println!("Query {} fragment(s)", data.fragments);
            }
            LoreEvent::RevisionRestoreFragmentProgress(data) => {
                progress_bar.set_progress(data.complete);
                progress_bar.set_max_progress(data.count);
                if debug {
                    print!("[Debug] Push {}/{} fragment(s)", data.complete, data.count);
                }
            }
            LoreEvent::RevisionRestoreFragmentEnd(data) if data.fragments > 0 => {
                println!("Pushed {} fragment(s)", data.fragments);
            }
            LoreEvent::RevisionRestoreRevision(data) => {
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

    return runtime().block_on(revision::restore(globals, restore_args, callback)) as u8;
}

pub fn handle_revision_cherry_pick(globals: LoreGlobalArgs, args: &RevisionCherryPickArgs) -> u8 {
    // Cherry-pick subcommand action
    if let Some(subcommand) = &args.subcommand {
        match subcommand {
            RevisionCherryPickCommands::Abort => handle_revision_cherry_pick_abort(globals),
            RevisionCherryPickCommands::Restart(sub_args) => {
                handle_revision_cherry_pick_restart(globals, sub_args)
            }
            RevisionCherryPickCommands::Unresolve(sub_args) => {
                handle_revision_cherry_pick_unresolve(globals, sub_args)
            }
            RevisionCherryPickCommands::Resolve(sub_args) => {
                handle_revision_cherry_pick_resolve(globals, sub_args)
            }
        }
    }
    // Default action - start cherry-pick
    else {
        let cherry_pick_args = revision::LoreRevisionCherryPickArgs {
            revision: LoreString::from(&args.revision),
            message: LoreString::from(&args.message),
            no_commit: args.no_commit as u8,
        };

        let debug = progress_debug();
        let progress_bar = ProgressBar::new(0);

        let callback = output_formatter().unwrap_or(Some(
            (Box::new(move |event: &LoreEvent| match event {
                LoreEvent::CherryPickStartBegin(_data) => {}
                LoreEvent::RevisionSyncProgress(data) => {
                    apply_sync_progress_to_bar(&progress_bar, data);
                    if debug {
                        println!("[Debug] {}", progress_info_display(data));
                    }
                }
                LoreEvent::CherryPickStartEnd(data) => {
                    println!("\r{}", merge_result_display(&data.stats));
                    println!("\rStaged cherry-picked repository state {}", data.signature);
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
                        "Committed cherry-picked repository state {} -> {}",
                        data.revision_number, data.revision
                    );
                }
                LoreEvent::CherryPickConflictFile(data) => {
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
        runtime().block_on(revision::cherry_pick(globals, cherry_pick_args, callback)) as u8
    }
}

fn handle_revision_cherry_pick_abort(globals: LoreGlobalArgs) -> u8 {
    let cherry_pick_abort_args = revision::LoreRevisionCherryPickAbortArgs {};

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::CherryPickAbortBegin(data) => {
                println!(
                    "Cherry-pick abort from staged revision {} to current revision {}",
                    data.state_staged_revision, data.state_current_revision
                );
            }
            LoreEvent::RevisionSyncProgress(data) => {
                apply_sync_progress_to_bar(&progress_bar, data);
                if debug {
                    println!("[Debug] {}", progress_info_display(data));
                }
            }
            LoreEvent::CherryPickAbortEnd(_data) => {
                println!("Cherry-pick abort reverted changes");
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(revision::cherry_pick_abort(
        globals,
        cherry_pick_abort_args,
        callback,
    )) as u8;
}

fn handle_revision_cherry_pick_unresolve(
    globals: LoreGlobalArgs,
    args: &RevisionCherryPickTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let cherry_pick_unresolve_args = revision::LoreRevisionCherryPickUnresolveArgs { paths };

    let count_atomic = AtomicU64::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::CherryPickUnresolveFile(data) => {
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

    return runtime().block_on(revision::cherry_pick_unresolve(
        globals,
        cherry_pick_unresolve_args,
        callback,
    )) as u8;
}

fn handle_revision_cherry_pick_restart(
    globals: LoreGlobalArgs,
    args: &RevisionCherryPickRestartArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.targets.paths, &args.targets.targets);

    let cherry_pick_restart_args = revision::LoreRevisionCherryPickRestartArgs { paths };

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

    return runtime().block_on(revision::cherry_pick_restart(
        globals,
        cherry_pick_restart_args,
        callback,
    )) as u8;
}

fn handle_revision_cherry_pick_resolve(
    globals: LoreGlobalArgs,
    args: &RevisionCherryPickResolveArgs,
) -> u8 {
    if let Some(subcommand) = &args.subcommand {
        // Resolve action
        match subcommand {
            RevisionCherryPickResolveCommands::Mine(sub_args) => {
                handle_revision_cherry_pick_resolve_mine(globals, sub_args)
            }
            RevisionCherryPickResolveCommands::Theirs(sub_args) => {
                handle_revision_cherry_pick_resolve_theirs(globals, sub_args)
            }
        }
    } else {
        let sub_args = RevisionCherryPickTargetArgs {
            paths: args.targets.paths.clone(),
            targets: args.targets.targets.clone(),
        };

        handle_revision_cherry_pick_resolve_impl(globals, &sub_args)
    }
}

fn handle_revision_cherry_pick_resolve_impl(
    globals: LoreGlobalArgs,
    args: &RevisionCherryPickTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let cherry_pick_resolve_args = revision::LoreRevisionCherryPickResolveArgs { paths };

    let count_atomic = AtomicU64::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::CherryPickResolveFile(data) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!(
                        "{}Resolved conflicts:{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                }
                println!("{}", data.path.as_str());

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

    return runtime().block_on(revision::cherry_pick_resolve(
        globals,
        cherry_pick_resolve_args,
        callback,
    )) as u8;
}

fn handle_revision_cherry_pick_resolve_mine(
    globals: LoreGlobalArgs,
    args: &RevisionCherryPickTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let cherry_pick_resolve_mine_args = revision::LoreRevisionCherryPickResolveMineArgs { paths };

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

    return runtime().block_on(revision::cherry_pick_resolve_mine(
        globals,
        cherry_pick_resolve_mine_args,
        callback,
    )) as u8;
}

fn handle_revision_cherry_pick_resolve_theirs(
    globals: LoreGlobalArgs,
    args: &RevisionCherryPickTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let cherry_pick_resolve_theirs_args =
        revision::LoreRevisionCherryPickResolveTheirsArgs { paths };

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

    return runtime().block_on(revision::cherry_pick_resolve_theirs(
        globals,
        cherry_pick_resolve_theirs_args,
        callback,
    )) as u8;
}

pub fn handle_revision_revert(globals: LoreGlobalArgs, args: &RevisionRevertArgs) -> u8 {
    // Revert subcommand action
    if let Some(subcommand) = &args.subcommand {
        match subcommand {
            RevisionRevertCommands::Abort => handle_revision_revert_abort(globals),
            RevisionRevertCommands::Restart(sub_args) => {
                handle_revision_revert_restart(globals, sub_args)
            }
            RevisionRevertCommands::Unresolve(sub_args) => {
                handle_revision_revert_unresolve(globals, sub_args)
            }
            RevisionRevertCommands::Resolve(sub_args) => {
                handle_revision_revert_resolve(globals, sub_args)
            }
        }
    }
    // Default action - start revert
    else {
        let revert_args = revision::LoreRevisionRevertArgs {
            revision: LoreString::from(&args.revision),
            message: LoreString::from(&args.message),
            no_commit: args.no_commit as u8,
        };

        let debug = progress_debug();
        let progress_bar = ProgressBar::new(0);

        let callback = output_formatter().unwrap_or(Some(
            (Box::new(move |event: &LoreEvent| match event {
                LoreEvent::RevertStartBegin(_data) => {}
                LoreEvent::RevisionSyncProgress(data) => {
                    apply_sync_progress_to_bar(&progress_bar, data);
                    if debug {
                        println!("[Debug] {}", progress_info_display(data));
                    }
                }
                LoreEvent::RevertStartEnd(data) => {
                    println!("\r{}", merge_result_display(&data.stats));
                    println!("\rStaged reverted repository state {}", data.signature);
                    if data.has_conflicts == 1 {
                        println!("Files in conflict:");
                    }
                }
                LoreEvent::RevisionCommitRevision(data) => {
                    println!(
                        "Committed reverted repository state {} -> {}",
                        data.revision_number, data.revision
                    );
                }
                LoreEvent::RevertConflictFile(data) => {
                    println!("{}{}{}", BranchStyles::CONFLICT, data.path, anstyle::Reset);
                }
                LoreEvent::RevertUnresolveFile(data) => {
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
        runtime().block_on(revision::revert(globals, revert_args, callback)) as u8
    }
}

fn handle_revision_revert_abort(globals: LoreGlobalArgs) -> u8 {
    let revert_abort_args = revision::LoreRevisionRevertAbortArgs {};

    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevertAbortBegin(data) => {
                println!(
                    "Revert abort from staged revision {} to current revision {}",
                    data.state_staged_revision, data.state_current_revision
                );
            }
            LoreEvent::RevisionSyncProgress(data) => {
                apply_sync_progress_to_bar(&progress_bar, data);
                if debug {
                    println!("[Debug] {}", progress_info_display(data));
                }
            }
            LoreEvent::RevertAbortEnd(_data) => {
                println!("Revert abort reverted changes");
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(revision::revert_abort(globals, revert_abort_args, callback)) as u8;
}

fn handle_revision_revert_unresolve(
    globals: LoreGlobalArgs,
    args: &RevisionRevertTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let revert_unresolve_args = revision::LoreRevisionRevertUnresolveArgs { paths };

    let count_atomic = AtomicU64::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevertUnresolveFile(data) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!("Marked unresolved:");
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

    return runtime().block_on(revision::revert_unresolve(
        globals,
        revert_unresolve_args,
        callback,
    )) as u8;
}

fn handle_revision_revert_restart(globals: LoreGlobalArgs, args: &RevisionRevertRestartArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.targets.paths, &args.targets.targets);

    let revert_restart_args = revision::LoreRevisionRevertRestartArgs { paths };

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

    return runtime().block_on(revision::revert_restart(
        globals,
        revert_restart_args,
        callback,
    )) as u8;
}

fn handle_revision_revert_resolve(globals: LoreGlobalArgs, args: &RevisionRevertResolveArgs) -> u8 {
    if let Some(subcommand) = &args.subcommand {
        // Resolve action
        match subcommand {
            RevisionRevertResolveCommands::Mine(sub_args) => {
                handle_revision_revert_resolve_mine(globals, sub_args)
            }
            RevisionRevertResolveCommands::Theirs(sub_args) => {
                handle_revision_revert_resolve_theirs(globals, sub_args)
            }
        }
    } else {
        let sub_args = RevisionRevertTargetArgs {
            paths: args.targets.paths.clone(),
            targets: args.targets.targets.clone(),
        };

        handle_revision_revert_resolve_impl(globals, &sub_args)
    }
}

fn handle_revision_revert_resolve_impl(
    globals: LoreGlobalArgs,
    args: &RevisionRevertTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let revert_resolve_args = revision::LoreRevisionRevertResolveArgs { paths };

    let count_atomic = AtomicU64::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RevertResolveFile(data) => {
                let count = count_atomic.load(Ordering::Relaxed);
                if count == 0 {
                    println!("Resolved conflicts:");
                }
                println!("{}", data.path.as_str());

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

    return runtime().block_on(revision::revert_resolve(
        globals,
        revert_resolve_args,
        callback,
    )) as u8;
}

fn handle_revision_revert_resolve_mine(
    globals: LoreGlobalArgs,
    args: &RevisionRevertTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let revert_resolve_mine_args = revision::LoreRevisionRevertResolveMineArgs { paths };

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

    return runtime().block_on(revision::revert_resolve_mine(
        globals,
        revert_resolve_mine_args,
        callback,
    )) as u8;
}

fn handle_revision_revert_resolve_theirs(
    globals: LoreGlobalArgs,
    args: &RevisionRevertTargetArgs,
) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let revert_resolve_theirs_args = revision::LoreRevisionRevertResolveTheirsArgs { paths };

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

    return runtime().block_on(revision::revert_resolve_theirs(
        globals,
        revert_resolve_theirs_args,
        callback,
    )) as u8;
}

pub fn handle_revision_metadata_clear(
    globals: LoreGlobalArgs,
    _args: &RevisionMetadataClearArgs,
) -> u8 {
    let clear_args = LoreRevisionMetadataClearArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::MetadataClearRevision(data) => {
                println!("Metadata cleared for revision {}", data.revision);
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(revision::metadata_clear(globals, clear_args, callback)) as u8;
}

pub fn handle_revision_metadata_get(globals: LoreGlobalArgs, args: &RevisionMetadataGetArgs) -> u8 {
    if args.key.is_some() {
        let get_args = LoreRevisionMetadataGetArgs {
            revision: LoreString::from(&args.revision),
            key: LoreString::from(&args.key),
        };

        let callback = output_formatter().unwrap_or(Some(
            (Box::new(move |event: &LoreEvent| match event {
                LoreEvent::Metadata(data) => print_metadata(data, None, None),
                LoreEvent::Complete(_) => {}
                LoreEvent::Maintenance(data) => {
                    util::handle_maintenance_event(data);
                }
                _ => (),
            }) as EventCallbackFn)
                .with_defaults(),
        ));

        return runtime().block_on(revision::metadata_get(globals, get_args, callback)) as u8;
    } else {
        let list_args = LoreRevisionMetadataListArgs {
            revision: LoreString::from(&args.revision),
        };

        let callback = output_formatter().unwrap_or(Some(
            (Box::new(move |event: &LoreEvent| match event {
                LoreEvent::Metadata(data) => print_metadata(data, None, None),
                LoreEvent::Complete(_) => {}
                LoreEvent::Maintenance(data) => {
                    util::handle_maintenance_event(data);
                }
                _ => (),
            }) as EventCallbackFn)
                .with_defaults(),
        ));

        runtime().block_on(revision::metadata_list(globals, list_args, callback)) as u8
    }
}

pub fn handle_revision_metadata_set(globals: LoreGlobalArgs, args: &RevisionMetadataSetArgs) -> u8 {
    let format = if args.binary {
        LoreMetadataType::Binary
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

    let set_args = LoreRevisionMetadataSetArgs {
        keys: LoreArray::from_vec(keys),
        values: LoreArray::from_vec(values),
        formats: LoreArray::from_vec(formats),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => print_metadata(data, None, None),
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(revision::metadata_set(globals, set_args, callback)) as u8;
}

pub fn handle_revision_metadata_commands(
    cmd: &RevisionMetadataCommands,
    globals: LoreGlobalArgs,
) -> u8 {
    match cmd {
        RevisionMetadataCommands::Clear(args) => handle_revision_metadata_clear(globals, args),
        RevisionMetadataCommands::Get(args) => handle_revision_metadata_get(globals, args),
        RevisionMetadataCommands::Set(args) => handle_revision_metadata_set(globals, args),
    }
}

pub fn handle_revision_commands(cmd: &RevisionCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        RevisionCommands::History(args) => handle_revision_history(globals, args),
        RevisionCommands::Info(args) => handle_revision_info(globals, args),
        RevisionCommands::Commit(args) => handle_revision_commit(globals, args),
        RevisionCommands::Amend(args) => handle_revision_amend(globals, args),
        RevisionCommands::Sync(args) => handle_revision_sync(globals, args),
        RevisionCommands::Diff(args) => handle_revision_diff(globals, args),
        RevisionCommands::Find(args) => handle_revision_find(globals, args),
        RevisionCommands::Restore(args) => handle_revision_restore(globals, args),
        RevisionCommands::CherryPick(args) => handle_revision_cherry_pick(globals, args),
        RevisionCommands::Revert(args) => handle_revision_revert(globals, args),
        RevisionCommands::Metadata(sub_cmd) => {
            handle_revision_metadata_commands(&sub_cmd.command, globals)
        }
        RevisionCommands::Bisect(args) => handle_revision_bisect(globals, args),
    }
}

fn store_list_data(
    data: &LoreRevisionHistoryEntryEventData,
    list_entry_data: Arc<Mutex<Vec<RevisionEntryData>>>,
) {
    let revision_number = data.revision_number;
    let signature = data.revision;
    let parent = if !data.parent[0].is_zero() {
        Some(data.parent[0])
    } else {
        None
    };
    let merge = if !data.parent[1].is_zero() {
        Some(data.parent[1])
    } else {
        None
    };
    list_entry_data.lock().push(RevisionEntryData {
        repository: Context::default(),
        revision_number,
        signature,
        parent,
        merge,
        metadata: None,
        delta: None,
    });
}

fn store_info_data(
    data: &LoreRevisionInfoEventData,
    list_entry_data: Arc<Mutex<Vec<RevisionEntryData>>>,
) {
    let revision_number = data.revision_number;
    let signature = data.revision;
    let parent = if !data.parent[0].is_zero() {
        Some(data.parent[0])
    } else {
        None
    };
    let merge = if !data.parent[1].is_zero() {
        Some(data.parent[1])
    } else {
        None
    };
    list_entry_data.lock().push(RevisionEntryData {
        repository: Context::default(),
        revision_number,
        signature,
        parent,
        merge,
        metadata: None,
        delta: None,
    });
}

fn store_info_delta_data(
    data: &LoreRevisionInfoDeltaEventData,
    info_entry_data: Arc<Mutex<Vec<RevisionEntryData>>>,
) {
    if let Some(entry) = info_entry_data.lock().last_mut() {
        let delta = RevisionEntryDelta {
            action: data.action_as_string_short().to_string(),
            path: data.path.to_string(),
            merged: data.merged_as_string_short().to_string(),
            metadata: None,
        };
        if let Some(entry_delta) = entry.delta.as_mut() {
            entry_delta.push(delta);
        } else {
            entry.delta = Some(vec![delta]);
        }
    }
}

fn store_commit_data(
    data: &LoreRevisionCommitRevisionEventData,
    commit_entry_data: Arc<Mutex<Vec<RevisionEntryData>>>,
) {
    let repository: Context = data.repository.into();
    let revision_number = data.revision_number;
    let signature = data.revision;
    let parent = if !data.parent.is_zero() {
        Some(data.parent)
    } else {
        None
    };
    let merge = if !data.parent_other.is_zero() {
        Some(data.parent_other)
    } else {
        None
    };

    commit_entry_data.lock().push(RevisionEntryData {
        repository,
        revision_number,
        signature,
        parent,
        merge,
        metadata: None,
        delta: None,
    });
}

fn store_metadata(
    data: &LoreMetadataEventData,
    list_entry_data: Arc<Mutex<Vec<RevisionEntryData>>>,
) {
    if let Some(entry) = list_entry_data.lock().last_mut() {
        if let Some(deltas) = entry.delta.as_mut()
            && let Some(delta) = deltas.last_mut()
        {
            if let Some(metadata) = delta.metadata.as_mut() {
                metadata.push(data.clone());
            } else {
                delta.metadata = Some(vec![data.clone()]);
            }
        } else if let Some(metadata) = entry.metadata.as_mut() {
            metadata.push(data.clone());
        } else {
            entry.metadata = Some(vec![data.clone()]);
        }
    }
}

fn resolve_revision_user_ids(
    globals: LoreGlobalArgs,
    data: Arc<Mutex<Vec<RevisionEntryData>>>,
) -> HashMap<String, String> {
    let mut user_ids: HashSet<String> = HashSet::new();

    // Add creator and committer ids
    for data in data.lock().iter() {
        if let Some(metadata) = data.metadata.as_ref() {
            for metadata in metadata.iter() {
                for id in user_ids_from_metadata(metadata) {
                    user_ids.insert(id.to_string());
                }
            }
        }
    }

    let user_ids: Vec<LoreString> = user_ids.iter().map(LoreString::from).collect();

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

    let result = runtime().block_on(auth::resolve_user_info(globals, auth_args, callback)) as u8;

    // If there was an error resolving names, don't bother doing anything else
    if result != 0 {
        return HashMap::default();
    }

    match Arc::try_unwrap(auth_data) {
        Ok(auth_data) => auth_data.into_inner(),
        Err(_) => HashMap::default(),
    }
}

/// Resolve a revision hash to the name of the branch the revision was
/// committed on. Two-step chain: `revision::info` exposes the revision's
/// metadata, from which we pull the `BRANCH` field (a `BranchId`); then
/// `branch::info` resolves that `BranchId` to a branch name.
///
/// Two caches, in the order the function consults them:
/// - `revision_to_branch` (revision hash → name): short-circuits the entire
///   chain on a repeat hash. Rarely hits in practice — each row of a history
///   listing has a distinct parent/merge hash — but the write is cheap.
/// - `branch_to_name` (`BranchId` → name): the cache that actually pays off.
///   Branches repeat heavily across rows (every merge into `main` reuses the
///   same target `BranchId`), so the inner `branch::info` call fires at most
///   once per unique branch in the listing.
///
/// Both caches store `Option<String>`, so failures are negative-cached.
fn branch_name_for_revision(
    globals: &LoreGlobalArgs,
    revision: Hash,
    revision_to_branch: &mut HashMap<Hash, Option<String>>,
    branch_to_name: &mut HashMap<Context, Option<String>>,
) -> Option<String> {
    if let Some(cached) = revision_to_branch.get(&revision) {
        return cached.clone();
    }

    let branch_id = fetch_branch_id_for_revision(globals.clone(), revision);
    let name = branch_id.and_then(|id| {
        if let Some(cached) = branch_to_name.get(&id) {
            return cached.clone();
        }
        let name = fetch_branch_name(globals.clone(), id);
        branch_to_name.insert(id, name.clone());
        name
    });

    revision_to_branch.insert(revision, name.clone());
    name
}

fn fetch_branch_id_for_revision(globals: LoreGlobalArgs, revision: Hash) -> Option<Context> {
    let info_args = LoreRevisionInfoArgs {
        revision: LoreString::from(revision.to_string()),
        delta: 0,
        metadata: 0,
    };

    let captured: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    // Sub-operation callback; safe to ignore error events.
    let callback =
        output_formatter().unwrap_or(Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                if data.key.as_str() == metadata::BRANCH
                    && let LoreMetadata::Context(id) = &data.value
                {
                    *captured_clone.lock() = Some(*id);
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        })));

    let result = runtime().block_on(revision::info(globals, info_args, callback)) as u8;
    if result != 0 {
        return None;
    }

    *captured.lock()
}

fn fetch_branch_name(globals: LoreGlobalArgs, branch_id: Context) -> Option<String> {
    let info_args = LoreBranchInfoArgs {
        branch: LoreString::from(branch_id.to_string()),
    };

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    // Sub-operation callback; safe to ignore error events.
    let callback =
        output_formatter().unwrap_or(Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::BranchInfo(data) => {
                *captured_clone.lock() = Some(data.name.to_string());
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        })));

    let result = runtime().block_on(branch::info(globals, info_args, callback)) as u8;
    if result != 0 {
        return None;
    }

    captured.lock().clone()
}

/// Build a "Merge X into Y" line for a merge auto-commit with no user message.
/// Returns `None` if the entry is not a merge auto-commit, or if neither branch
/// name can be resolved (in which case the caller should keep the original
/// empty-message behavior).
fn auto_merge_message(
    globals: &LoreGlobalArgs,
    entry: &RevisionEntryData,
    revision_to_branch: &mut HashMap<Hash, Option<String>>,
    branch_to_name: &mut HashMap<Context, Option<String>>,
) -> Option<String> {
    if !entry.is_merge() {
        return None;
    }
    if !entry.message().unwrap_or_default().is_empty() {
        return None;
    }

    let parent = entry.parent?;
    let merge = entry.merge?;

    let target = branch_name_for_revision(globals, parent, revision_to_branch, branch_to_name);
    let source = branch_name_for_revision(globals, merge, revision_to_branch, branch_to_name);

    if target.is_none() && source.is_none() {
        return None;
    }

    let source = source.unwrap_or_else(|| merge.to_string());
    let target = target.unwrap_or_else(|| parent.to_string());

    Some(format!("Merge {source} into {target}"))
}
