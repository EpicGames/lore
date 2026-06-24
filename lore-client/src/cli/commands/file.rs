// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use clap::Args;
use clap::Subcommand;
use lore::dependency;
use lore::file;
use lore::file::LoreFileObliterateArgs;
use lore::interface::*;
use lore::runtime;
use lore_revision::file::diff::DEFAULT_CONTEXT_LINES;
use parking_lot::Mutex;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::pager::Pager;
use crate::print;
use crate::println;
use crate::progress_bar::ProgressBar;
use crate::progress_bar::progress_debug;
use crate::progress_bar::sync::apply_sync_progress_to_bar;
use crate::styling::BranchStyles;
use crate::styling::CommonStyles;
use crate::styling::FileActionStyle;
use crate::styling::FileDiffStyles;
use crate::util;
use crate::util::convert_paths_and_targets;
use crate::util::progress_info_display;

#[derive(Args)]
pub struct FileArgs {
    #[command(subcommand)]
    pub command: FileCommands,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct FilePathsTargetsArgs {
    /// Any number of paths or files
    #[clap(value_name = "paths", num_args = 1.., )]
    paths: Option<Vec<String>>,

    /// Path to a targets file containing all the paths to all files
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
#[group(required = false, multiple = false)]
pub struct FileOptionalPathsTargetsArgs {
    /// Any number of paths or files
    #[clap(value_name = "paths", num_args = 1.., )]
    paths: Option<Vec<String>>,

    /// Path to a targets file containing all the paths to all files
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
pub struct FileHashArgs {
    /// Any number of paths or files to unstage
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
pub struct FileMetadataArgs {
    #[command(subcommand)]
    pub command: FileMetadataCommands,
}

#[derive(Args)]
pub struct FileMetadataClearArgs {
    /// File path to clear metadata for
    path: String,
}

#[derive(Args)]
pub struct FileMetadataGetArgs {
    /// File to get metadata for
    path: String,

    /// Attribute to get metadata for
    #[clap(value_name = "key")]
    key: Option<String>,

    /// Revision to get metadata for
    #[clap(long, value_name = "revision")]
    revision: Option<String>,
}

#[derive(Args)]
pub struct FileMetadataSetArgs {
    /// File path to set metadata on
    path: String,

    /// Metadata key/value pairs
    #[clap(value_name = "pairs", num_args = 1..)]
    pairs: Option<Vec<String>>,

    /// Indicator that values are paths to files
    #[clap(long, action)]
    binary: bool,
}

#[derive(Subcommand)]
pub enum FileMetadataCommands {
    /// Clear metadata for a staged file
    Clear(FileMetadataClearArgs),

    /// Get metadata from a file
    Get(FileMetadataGetArgs),

    /// Set metadata on for a staged file
    Set(FileMetadataSetArgs),
}

#[derive(Args)]
pub struct FileDependencyArgs {
    #[command(subcommand)]
    pub command: FileDependencyCommands,
}

#[derive(Subcommand)]
pub enum FileDependencyCommands {
    /// Add dependency edges from a source file to one or more dependency files
    Add(FileDependencyAddArgs),

    /// Remove dependency edges from a source file to one or more dependency files
    Remove(FileDependencyRemoveArgs),

    /// List dependencies or dependents for files
    List(FileDependencyListArgs),
}

#[derive(Args)]
pub struct FileDependencyAddArgs {
    /// Source file that depends on the listed dependencies
    source: String,

    /// One or more dependency file paths
    #[clap(value_name = "dependencies", num_args = 1..)]
    dependencies: Vec<String>,

    /// Tags to apply to all added dependency edges
    #[clap(long = "tag", value_name = "tag", num_args = 1..)]
    tags: Option<Vec<String>>,

    /// Skip cycle detection
    #[clap(long, action)]
    force: bool,
}

#[derive(Args)]
pub struct FileDependencyRemoveArgs {
    /// Source file to remove dependencies from
    source: String,

    /// One or more dependency file paths to remove
    #[clap(value_name = "dependencies", num_args = 1..)]
    dependencies: Vec<String>,

    /// Remove only specific tags instead of entire edges
    #[clap(long = "tag", value_name = "tag", num_args = 1..)]
    tags: Option<Vec<String>>,
}

#[derive(Args)]
pub struct FileDependencyListArgs {
    /// Paths to list dependencies for (all files if omitted)
    #[clap(value_name = "paths", num_args = 0..)]
    paths: Option<Vec<String>>,

    /// List dependents instead of dependencies
    #[clap(long, action)]
    reverse: bool,

    /// Recursively resolve transitive dependencies
    #[clap(long, action)]
    recursive: bool,

    /// Filter by tag
    #[clap(long = "tag", value_name = "tag", num_args = 1..)]
    tags: Option<Vec<String>>,

    /// Maximum recursion depth (0 = unlimited)
    #[clap(long, value_name = "limit", default_value = "0")]
    depth: u32,

    /// Revision to query (defaults to staged/current)
    #[clap(long, value_name = "revision")]
    revision: Option<String>,
}

#[derive(Args)]
pub struct FileInfoArgs {
    #[clap(flatten)]
    paths: FilePathsTargetsArgs,

    /// Revision to get info from.
    #[clap(long, value_name = "revision")]
    revision: Option<String>,

    /// If given, calculate the local file system size and hash based on the current local filter.
    #[clap(long, action, required = false)]
    pub local: bool,

    /// If given, calculate the repository size based on the current local filter.
    #[clap(long, action, required = false)]
    pub filtered: bool,
}

/// Parse the `--context`/`-U` value, rejecting non-numeric and negative input
/// with one clear, actionable message. `u32::from_str` yields the unhelpful
/// "invalid digit found in string", and negatives must be caught explicitly.
fn parse_context_lines(s: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .map_err(|_err| format!("expected a non-negative integer; got '{s}'"))
}

#[derive(Args)]
pub struct FileDiffArgs {
    /// Optional signature of the source revision to diff from, by default the current revision
    #[clap(long, value_name = "revision_source")]
    source: Option<String>,

    /// Optional signature of the target revision to diff to, by default the current file system state
    #[clap(long, value_name = "revision_target")]
    target: Option<String>,

    /// If given, produce three-way merge output with conflict markers instead of a two-way unified diff
    #[clap(long, action, required = false)]
    diff3: bool,

    /// Number of unchanged context lines to show around each hunk
    #[clap(
        long,
        short = 'U',
        value_name = "n",
        default_value_t = DEFAULT_CONTEXT_LINES,
        value_parser = parse_context_lines,
        allow_negative_numbers = true
    )]
    context: u32,

    /// Treat lines that differ only in trailing whitespace as unchanged
    #[clap(long, action, required = false)]
    ignore_space_at_eol: bool,

    /// Collapse runs of internal whitespace to a single space before comparing
    #[clap(long, action, required = false)]
    ignore_space_change: bool,

    #[clap(flatten)]
    paths: FileOptionalPathsTargetsArgs,
}

#[derive(clap::ValueEnum, Clone, Default)]
pub enum FileStageCase {
    /// Generate error on case mismatch
    #[default]
    Error = 0,
    /// Keep current case in repository (update file system)
    Keep,
    /// Rename case in repository (keep file system)
    Rename,
}

impl FileStageCase {
    pub fn to_core_arg(&self) -> u32 {
        match self {
            Self::Keep => 1,
            Self::Rename => 2,
            Self::Error => 0,
        }
    }
}

#[derive(Args)]
#[command(subcommand_negates_reqs = true)]
#[group(required = false, multiple = false)]
pub struct FileStageCommandArgs {
    /// Move or merge
    #[command(subcommand)]
    subcommand: Option<FileStageCommands>,
}

#[derive(Args)]
pub struct FileStageArgs {
    /// Case change handling
    #[clap(long, value_name = "case")]
    case: Option<FileStageCase>,

    /// Walk the filesystem under the given paths to detect modified,
    /// added, and deleted files.
    ///
    /// Detected changes are marked dirty and staged in a single pass.
    /// Use this when changes were made externally (without going
    /// through `lore dirty`), or to recover after losing track of
    /// dirty state. Equivalent in effect to running
    /// `lore status --scan` followed by `lore stage`, but performed
    /// in one traversal.
    ///
    /// Without `--scan`, directory staging stages only files already
    /// marked dirty under that directory — mark them first with
    /// `lore dirty <paths>`, or run `lore status --scan` to reconcile
    /// dirty flags across a tree. Single-file stage paths are always
    /// checked against the filesystem regardless of this flag.
    #[clap(long, action)]
    scan: bool,

    #[clap(flatten)]
    paths: FilePathsTargetsArgs,

    #[clap(flatten)]
    stage: FileStageCommandArgs,
}

#[derive(Args)]
pub struct FileStageMergeArgs {
    #[clap(flatten)]
    paths: FilePathsTargetsArgs,
}

#[derive(Args)]
pub struct FileStageMoveArgs {
    /// Original path of file
    #[clap(value_name = "from")]
    from: String,

    /// New path of file
    #[clap(value_name = "to")]
    to: String,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct FileUnstageArgs {
    /// Any number of paths or files to unstage
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
pub struct FileResetArgs {
    /// Delete untracked files
    #[clap(long, action)]
    purge: bool,

    #[clap(flatten)]
    paths_targets: FilePathsTargetsArgs,

    /// Revision to reset files to
    #[clap(long, value_name = "revision")]
    revision: Option<String>,

    /// If given, the files will be reset to the last point of merge
    /// from this branch, or the branch point from this branch if no
    /// merge has been performed.
    #[clap(long, value_name = "branch")]
    last_merged_from: Option<String>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct FileDumpArgs {
    /// Address of a blob
    #[clap(long)]
    address: Option<String>,

    /// Path to a file
    #[clap(long)]
    path: Option<String>,
}

#[derive(Args)]
pub struct FileHistoryArgs {
    /// File path to get revisions for
    path: String,

    /// Revision to start from
    #[clap(long, value_name = "revision", conflicts_with = "branch")]
    revision: Option<String>,

    /// Show branch revisions
    #[clap(long, value_name = "branch")]
    branch: Option<String>,

    /// Number of revisions to show
    length: Option<u32>,

    /// Number of revisions to search initially
    #[clap(long, value_name = "depth")]
    depth: Option<u32>,

    /// Output each revision on one line only
    #[clap(long, action)]
    pub oneline: bool,
}

#[derive(Args)]
pub struct FileWriteArgs {
    /// Address of a blob
    #[clap(long)]
    address: Option<String>,

    /// Path to a file
    #[clap(long)]
    path: Option<String>,

    /// Revision specifier
    #[clap(long, requires = "path")]
    revision: Option<String>,

    /// Path to a destination
    #[clap(long)]
    output: String,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct FileObliterateArgs {
    /// Address of a blob
    #[clap(long)]
    address: Option<String>,

    /// Path to a file
    #[clap(long)]
    path: Option<String>,
}

#[derive(Args)]
pub struct FileDirtyArgs {
    #[clap(flatten)]
    paths: FileDirtyPathsArgs,

    #[clap(flatten)]
    dirty: FileDirtyCommandArgs,
}

/// Paths for dirty command — NOT required at group level since subcommands (move/copy) have
/// their own path arguments. Required-ness is enforced in the handler when no subcommand is used.
#[derive(Args)]
pub struct FileDirtyPathsArgs {
    /// Any number of paths or files
    #[clap(value_name = "paths", num_args = 1..)]
    paths: Option<Vec<String>>,

    /// Path to a targets file containing all the paths to all files
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
pub struct FileDirtyCommandArgs {
    #[clap(subcommand)]
    subcommand: Option<FileDirtyCommands>,
}

#[derive(Subcommand)]
pub enum FileDirtyCommands {
    /// Mark a file as moved (dirty)
    Move(FileDirtyMoveArgs),
    /// Mark a file as copied (dirty)
    Copy(FileDirtyCopyArgs),
}

#[derive(Args)]
pub struct FileDirtyMoveArgs {
    /// Original path of file
    #[clap(value_name = "from")]
    from: String,

    /// New path of file
    #[clap(value_name = "to")]
    to: String,
}

#[derive(Args)]
pub struct FileDirtyCopyArgs {
    /// Source path of file
    #[clap(value_name = "from")]
    from: String,

    /// Destination path of copy
    #[clap(value_name = "to")]
    to: String,
}

#[derive(Subcommand)]
pub enum FileStageCommands {
    /// Move or rename a file or directory
    Move(FileStageMoveArgs),

    /// Stage as a merge
    Merge(FileStageMergeArgs),
}

#[derive(Subcommand)]
pub enum FileCommands {
    // Status,
    /// Get info about the given file or directory
    Info(FileInfoArgs),
    /// Manage metadata of a given file or directory
    Metadata(FileMetadataArgs),
    /// Manage file dependencies
    Dependency(FileDependencyArgs),
    /// Stage changes for commit.
    ///
    /// Directory paths (including `.`) stage only files already marked
    /// dirty under that directory; clean or unmarked files are
    /// skipped. Mark files first with `lore file dirty` (or
    /// `lore status --scan` to reconcile dirty flags in bulk), or
    /// pass `--scan` here to walk the filesystem and stage in one
    /// pass.
    ///
    /// Specific file paths are checked against the filesystem and
    /// staged if content differs from the current revision,
    /// regardless of their dirty flag.
    ///
    /// `--scan` walks the filesystem under the given paths, marks
    /// every detected modification/add/delete dirty, and stages them
    /// in one step.
    Stage(FileStageArgs),
    /// Mark files as dirty so they show up in `lore status` and get
    /// picked up by directory-scoped `lore stage` (no content is read
    /// or staged).
    ///
    /// Use when files were changed externally and you want to notify
    /// Lore of specific paths without performing a full filesystem
    /// walk. For bulk reconciliation across a tree, prefer
    /// `lore status --scan` or `lore stage --scan`.
    Dirty(FileDirtyArgs),
    /// Unstage changes to a file or directory
    Unstage(FileUnstageArgs),
    /// Reset changes to a path or file to the current revision, discarding your local changes
    Reset(FileResetArgs),
    // Delete,
    /// Obliterate a file or fragment
    Obliterate(FileObliterateArgs),
    /// Dump file information from store
    #[clap(hide = true)]
    Dump(FileDumpArgs),
    /// List revisions of a file
    History(FileHistoryArgs),
    /// Show differences between two revisions of a file
    Diff(FileDiffArgs),
    /// Write data to a specific location
    Write(FileWriteArgs),
    /// Hash a local file
    Hash(FileHashArgs),
}

pub fn format_key(key: &str) -> &str {
    match key {
        "asset-name" => "AssetName",
        "asset-class" => "AssetClass",
        "thumbnail" => "Thumbnail",
        "meta-json" => "MetaJson",
        metadata::CREATED_BY => "Creator",
        metadata::COMMITTED_BY => "Committer",
        metadata::REVIEWED_BY => "Reviewer",
        metadata::MERGED_BY => "Merger",
        metadata::TIMESTAMP => "Date",
        metadata::BRANCH => "Branch",
        metadata::MESSAGE => "   ",
        metadata::P4_CHANGELIST => "Changelist",
        metadata::RESTORED_FROM => "Restored",
        _ => key,
    }
}

pub fn format_value(key: &str, value: &str, user_map: Option<&HashMap<String, String>>) -> String {
    match key {
        metadata::CREATED_BY
        | metadata::COMMITTED_BY
        | metadata::REVIEWED_BY
        | metadata::MERGED_BY => {
            // Expect comma separated list of user identities
            if let Some(user_map) = user_map.as_ref() {
                let mut resolved = vec![];
                for item in value.split(" ,\t\n\r") {
                    if let Some(mapped) = user_map.get(item) {
                        resolved.push(mapped.clone());
                        continue;
                    }
                    resolved.push(item.to_string());
                }
                resolved.join(", ")
            } else {
                value.to_string()
            }
        }
        _ => value.to_string(),
    }
}

pub fn user_ids_from_metadata(metadata: &LoreMetadataEventData) -> Vec<LoreString> {
    let mut ids = vec![];
    match metadata.key.as_str() {
        metadata::CREATED_BY
        | metadata::COMMITTED_BY
        | metadata::REVIEWED_BY
        | metadata::MERGED_BY => {
            if let LoreMetadata::String(value) = &metadata.value {
                // Expect comma separated list of user identities
                for item in value.as_str().split(" ,\t\n\r") {
                    ids.push(item.into());
                }
            }
        }
        _ => {}
    }
    ids
}

pub fn print_metadata(
    metadata: &LoreMetadataEventData,
    user_map: Option<&HashMap<String, String>>,
    empty_message_fallback: Option<&str>,
) {
    let key = format_key(metadata.key.as_str());
    match &metadata.value {
        LoreMetadata::Address(value) => {
            println!(
                "{}{key:<10}:{} {value}",
                CommonStyles::HEADERS,
                anstyle::Reset,
            );
        }
        LoreMetadata::Binary(value) => {
            println!(
                "{}{key:<10}:{} <{} bytes>",
                CommonStyles::HEADERS,
                anstyle::Reset,
                value.length
            );
        }
        LoreMetadata::Boolean(value) => {
            println!(
                "{}{key:<10}:{} {value}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
        LoreMetadata::Context(value) => {
            println!(
                "{}{key:<10}:{} {value}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
        LoreMetadata::Hash(value) => {
            println!(
                "{}{key:<10}:{} {value}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
        LoreMetadata::Numeric(value) => {
            if metadata.key.as_str() == metadata::TIMESTAMP
                && let Some(timestamp) = DateTime::from_timestamp_millis(*value as i64)
            {
                println!(
                    "{}{key:<10}:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    timestamp.to_rfc2822()
                );
                return;
            }
            println!(
                "{}{key:<10}:{} {value}",
                CommonStyles::HEADERS,
                anstyle::Reset
            );
        }
        LoreMetadata::String(value) => {
            if metadata.key.as_str() == metadata::MESSAGE {
                let text = if value.as_str().is_empty() {
                    empty_message_fallback.unwrap_or("")
                } else {
                    value.as_str()
                };
                for line in text.lines() {
                    println!("    {line}");
                }
            } else {
                let value = format_value(metadata.key.as_str(), value.as_str(), user_map);
                println!(
                    "{}{key:<10}:{} {value}",
                    CommonStyles::HEADERS,
                    anstyle::Reset
                );
            }
        }
    }
}

pub fn handle_file_info(globals: LoreGlobalArgs, args: &FileInfoArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths.paths, &args.paths.targets);
    let local = args.local;
    let filtered = args.filtered;
    let info_args = LoreFileInfoArgs {
        paths,
        revision: args.revision.as_ref().into(),
        local: args.local as u8,
        filtered: args.filtered as u8,
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileInfo(data) => {
                println!(
                    "{}Path:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.path.as_str()
                );
                println!(
                    "{}Type:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    if data.is_dir != 0 { "dir" } else { "file" }
                );
                println!(
                    "{}Size:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.size
                );
                println!(
                    "{}Mode:{}    {:03o}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.mode
                );
                println!(
                    "{}Context:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.context
                );
                println!(
                    "{}Hash:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.hash
                );
                if local {
                    println!(
                        "{}Local size:{}    {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.local_size
                    );
                    println!(
                        "{}Local hash:{}    {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.local_hash
                    );
                }
                if filtered {
                    println!(
                        "{}Filtered size:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.filter_size
                    );
                }
                let mut status = vec![];
                if data.flag_added != 0 {
                    status.push("Added");
                }
                if data.flag_modified != 0 {
                    status.push("Modified");
                }
                if data.flag_deleted != 0 {
                    status.push("Deleted");
                }
                if data.flag_conflict != 0 {
                    status.push("Conflict");
                }
                if status.is_empty() {
                    status.push("-");
                }
                println!(
                    "{}Status:{}  {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    status.join(", ")
                );
            }
            LoreEvent::Metadata(data) => print_metadata(data, None, None),
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(file::info(globals, info_args, callback)) as u8;
}

pub fn handle_file_diff(globals: LoreGlobalArgs, args: &FileDiffArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths.paths, &args.paths.targets);

    let diff_args = LoreFileDiffArgs {
        paths,
        source_revision: LoreString::from(&args.source),
        target_revision: LoreString::from(&args.target),
        diff3: args.diff3 as u8,
        context_lines: args.context,
        ignore_whitespace_eol: args.ignore_space_at_eol as u8,
        ignore_whitespace_inline: args.ignore_space_change as u8,
    };

    let _pager = Pager::new();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileDiff(data) => {
                // Always show unified diff patches for all actions
                match data.action {
                    LoreFileAction::Keep
                    | LoreFileAction::Delete
                    | LoreFileAction::Add
                    | LoreFileAction::Move => {
                        // Show patch content
                        println!();
                        println!(
                            "{}{}{}",
                            CommonStyles::HEADERS,
                            data.path.as_str(),
                            anstyle::Reset
                        );
                        let patch_str = data.patch.as_str();
                        let patch_lines = patch_str.lines();
                        for line in patch_lines {
                            let style = if line.starts_with("<<<<<<<")
                                || line.starts_with("|||||||")
                                || line.starts_with("=======")
                                || line.starts_with(">>>>>>>")
                            {
                                BranchStyles::CONFLICT
                            } else {
                                match line.chars().next() {
                                    Some('+') => FileDiffStyles::ADDITIONS,
                                    Some('-') => FileDiffStyles::DELETIONS,
                                    None | Some(_) => FileDiffStyles::DEFAULT,
                                }
                            };
                            println!("{}{}{}", style, line, anstyle::Reset);
                        }
                        println!();
                    }
                    LoreFileAction::Copy => {
                        // Status format for Copy
                        println!(
                            "{}{}{} {}",
                            FileActionStyle::from_action(data.action),
                            data.action.as_string_short(),
                            anstyle::Reset,
                            data.path.as_str()
                        );
                    }
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(file::diff(globals, diff_args, callback)) as u8;
}

pub fn handle_file_hash(globals: LoreGlobalArgs, args: &FileHashArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let hash_args = LoreFileHashArgs { paths };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileHash(data) => {
                println!(
                    "{}Path:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.path.as_str()
                );
                println!(
                    "{}Size:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.size
                );
                println!(
                    "{}Hash:{}    {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.hash
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(file::hash(globals, hash_args, callback)) as u8;
}

pub fn handle_file_metadata_clear(globals: LoreGlobalArgs, args: &FileMetadataClearArgs) -> u8 {
    let path = LoreString::from(&args.path);

    let clear_args = LoreFileMetadataClearArgs { path: path.clone() };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::MetadataClearFile(data) => {
                println!("Metadata cleared for file {}", data.path.as_str());
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(file::metadata_clear(globals, clear_args, callback)) as u8;
}

pub fn handle_file_metadata_get(globals: LoreGlobalArgs, args: &FileMetadataGetArgs) -> u8 {
    if args.key.is_some() {
        let get_args = LoreFileMetadataGetArgs {
            path: args.path.as_str().into(),
            revision: args.revision.as_ref().into(),
            key: args.key.as_ref().into(),
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

        return runtime().block_on(file::metadata_get(globals, get_args, callback)) as u8;
    } else {
        let list_args = LoreFileMetadataListArgs {
            path: args.path.as_str().into(),
            revision: args.revision.as_ref().into(),
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

        runtime().block_on(file::metadata_list(globals, list_args, callback)) as u8
    }
}

pub fn handle_file_metadata_set(globals: LoreGlobalArgs, args: &FileMetadataSetArgs) -> u8 {
    let path = LoreString::from(&args.path);
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

    let mut paths = vec![];
    let mut keys = vec![];
    let mut values = vec![];
    let mut formats = vec![];
    let mut entries = vec![];
    for (index, element) in elements.as_slice().iter().enumerate() {
        if index.is_multiple_of(2) {
            keys.push(element.clone());
        } else {
            values.push(element.clone());
            formats.push(format);
        }
    }

    paths.push(path);
    entries.push(keys.len() as u32);

    let set_args = LoreFileMetadataSetArgs {
        paths: LoreArray::from_vec(paths),
        keys: LoreArray::from_vec(keys),
        values: LoreArray::from_vec(values),
        formats: LoreArray::from_vec(formats),
        entries: LoreArray::from_vec(entries),
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

    return runtime().block_on(file::metadata_set(globals, set_args, callback)) as u8;
}

fn stage_info_display(count: &LoreFileStageCountData) -> String {
    if count.total_count == 0 {
        return "No changes staged".to_string();
    };

    let dir_display = if count.directory_add_count
        + count.directory_delete_count
        + count.directory_move_count
        > 0
    {
        format!(
            " {} directories ({} added, {} deleted, {} moved)",
            count.directory_add_count + count.directory_delete_count + count.directory_move_count,
            count.directory_add_count,
            count.directory_delete_count,
            count.directory_move_count,
        )
    } else {
        String::new()
    };

    let file_display = if count.file_modify_count
        + count.file_add_count
        + count.file_delete_count
        + count.file_move_count
        > 0
    {
        let mut file_display = String::new();
        if !dir_display.is_empty() {
            file_display.push_str(", ");
        }
        file_display.push_str(
            format!(
                " {} files ({} modified, {} added, {} deleted, {} moved)",
                count.file_modify_count
                    + count.file_add_count
                    + count.file_delete_count
                    + count.file_move_count,
                count.file_modify_count,
                count.file_add_count,
                count.file_delete_count,
                count.file_move_count,
            )
            .as_str(),
        );
        file_display
    } else {
        String::new()
    };

    format!("Staging{dir_display}{file_display}")
}

pub fn handle_file_stage(globals: LoreGlobalArgs, args: &FileStageArgs) -> u8 {
    let debug = progress_debug();
    let progress_bar = ProgressBar::new(0);
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileStageBegin(_data) => {
                println!("Staging file system changes");
            }
            LoreEvent::FileStageProgress(data) if debug && data.count.total_count > 0 => {
                println!("[Debug] {}", stage_info_display(&data.count));
            }
            LoreEvent::RevisionSyncProgress(data) => {
                if debug {
                    println!("[Debug] {}", progress_info_display(data));
                }
                apply_sync_progress_to_bar(&progress_bar, data);
            }
            LoreEvent::FileStageEnd(data) => {
                println!("{}", stage_info_display(&data.count));
            }
            LoreEvent::FileStageRevision(data) => {
                println!("Staged repository state {}", data.revision);
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::PathIgnore(data) => {
                util::handle_path_ignore_event(data);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    // Standard stage
    if args.stage.subcommand.is_none() {
        let paths = convert_paths_and_targets(&args.paths.paths, &args.paths.targets);

        let stage_args = LoreFileStageArgs {
            paths,
            case_change: args.case.clone().unwrap_or_default().to_core_arg(),
            scan: u8::from(args.scan),
        };

        return runtime().block_on(file::stage(globals, stage_args, callback)) as u8;
    }

    // Stage move
    match args.stage.subcommand.as_ref().unwrap() {
        FileStageCommands::Move(sub_args) => {
            let stage_args = LoreFileStageMoveArgs {
                from_path: LoreString::from(&sub_args.from),
                to_path: LoreString::from(&sub_args.to),
            };

            return runtime().block_on(file::stage_move(globals, stage_args, callback)) as u8;
        }

        FileStageCommands::Merge(sub_args) => {
            let paths = convert_paths_and_targets(&sub_args.paths.paths, &sub_args.paths.targets);

            let stage_args = LoreFileStageMergeArgs { paths };

            return runtime().block_on(file::stage_merge(globals, stage_args, callback)) as u8;
        }
    }
}

pub fn handle_file_dirty(globals: LoreGlobalArgs, args: &FileDirtyArgs) -> u8 {
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::PathIgnore(data) => {
                util::handle_path_ignore_event(data);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    // Standard dirty (paths)
    if args.dirty.subcommand.is_none() {
        let paths = convert_paths_and_targets(&args.paths.paths, &args.paths.targets);
        if paths.is_empty() {
            println!("error: paths or --targets required for dirty command");
            return 1;
        }

        let dirty_args = LoreFileDirtyArgs { paths };

        return runtime().block_on(file::dirty(globals, dirty_args, callback)) as u8;
    }

    // Dirty move/copy subcommands
    match args.dirty.subcommand.as_ref().unwrap() {
        FileDirtyCommands::Move(sub_args) => {
            let dirty_args = LoreFileDirtyMoveArgs {
                from_path: LoreString::from(&sub_args.from),
                to_path: LoreString::from(&sub_args.to),
            };

            runtime().block_on(file::dirty_move(globals, dirty_args, callback)) as u8
        }

        FileDirtyCommands::Copy(sub_args) => {
            let dirty_args = LoreFileDirtyCopyArgs {
                from_path: LoreString::from(&sub_args.from),
                to_path: LoreString::from(&sub_args.to),
            };

            runtime().block_on(file::dirty_copy(globals, dirty_args, callback)) as u8
        }
    }
}

fn unstage_info_display(count: &LoreFileUnstageCountData) -> String {
    format!(
        "Unstaging {} ({} directories, {} files), discarded {} ({} directories, {} files)",
        count.directory_unstaged_count + count.file_unstaged_count,
        count.directory_unstaged_count,
        count.file_unstaged_count,
        count.directory_discarded_count + count.file_discarded_count,
        count.directory_discarded_count,
        count.file_discarded_count
    )
}

pub fn handle_file_unstage(globals: LoreGlobalArgs, args: &FileUnstageArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths, &args.targets);

    let unstage_args = LoreFileUnstageArgs { paths };

    // LoreFileUnstageCountData.total_count is a running cumulative count, not a
    // fixed total, so we use a spinner and update the message on each Progress event.
    let bar = ProgressBar::new_spinner("Unstaging...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileUnstageBegin(_data) => {
                println!("Unstaging file system changes");
            }
            LoreEvent::FileUnstageProgress(data) => {
                bar.set_message(unstage_info_display(&data.count));
            }
            LoreEvent::FileUnstageEnd(data) => {
                if data.count.total_count > 0 {
                    println!("{}", unstage_info_display(&data.count));
                } else {
                    println!("No changes unstaged");
                }
            }
            LoreEvent::FileUnstageRevision(data) => {
                println!("Unstaged repository state {}", data.revision);
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::PathIgnore(data) => {
                util::handle_path_ignore_event(data);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(file::unstage(globals, unstage_args, callback)) as u8;
}

fn reset_info_total(count: &LoreFileResetCountData) -> u64 {
    count.directory_reset_count
        + count.directory_delete_count
        + count.file_reset_count
        + count.file_delete_count
}

fn reset_info_display(count: &LoreFileResetCountData) -> String {
    format!(
        "Reset {} ({} directories, {} files), deleted {} ({} directories, {} files)",
        count.directory_reset_count + count.file_reset_count,
        count.directory_reset_count,
        count.file_reset_count,
        count.directory_delete_count + count.file_delete_count,
        count.directory_delete_count,
        count.file_delete_count
    )
}

pub fn handle_file_reset(globals: LoreGlobalArgs, args: &FileResetArgs) -> u8 {
    let paths = convert_paths_and_targets(&args.paths_targets.paths, &args.paths_targets.targets);

    // LoreFileResetCountData has no file_total field, so we use a spinner and
    // update the message on each Progress event via set_message.
    let bar = ProgressBar::new_spinner("Resetting...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileResetBegin(_data) => {
                println!("Resetting file system changes");
            }
            LoreEvent::FileResetProgress(data) => {
                bar.set_message(reset_info_display(&data.count));
            }
            LoreEvent::FileResetEnd(data) => {
                if reset_info_total(&data.count) > 0 {
                    println!("{}", reset_info_display(&data.count));
                } else {
                    println!("No files reset");
                }
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::PathIgnore(data) => {
                util::handle_path_ignore_event(data);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    if let Some(branch) = args.last_merged_from.as_ref() {
        let reset_last_merged_args = LoreFileResetToLastMergedArgs {
            paths,
            purge: args.purge.into(),
            branch: branch.into(),
        };

        return runtime().block_on(file::reset_to_last_merged(
            globals,
            reset_last_merged_args,
            callback,
        )) as u8;
    } else {
        let reset_args = LoreFileResetArgs {
            paths,
            revision: args.revision.as_ref().into(),
            purge: args.purge.into(),
        };

        return runtime().block_on(file::reset(globals, reset_args, callback)) as u8;
    }
}

#[derive(Default)]
struct OnelineState {
    pending_revision: Mutex<Option<u64>>,
    pending_message: Mutex<Option<String>>,
}

impl OnelineState {
    fn buffer_entry(&self, revision_number: u64) {
        self.flush();
        *self.pending_revision.lock() = Some(revision_number);
        *self.pending_message.lock() = Some(String::new());
    }

    fn set_message(&self, message: String) {
        *self.pending_message.lock() = Some(message);
    }

    fn flush(&self) {
        if let Some(revision_number) = self.pending_revision.lock().take() {
            let message = self.pending_message.lock().take().unwrap_or_default();
            println!("{revision_number} {message}");
        }
    }
}

pub fn handle_file_history(globals: LoreGlobalArgs, args: &FileHistoryArgs) -> u8 {
    let first_entry: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
    let oneline = args.oneline;

    let log_args = LoreFileHistoryArgs {
        path: args.path.as_str().into(),
        revision: args.revision.as_ref().into(),
        branch: args.branch.as_ref().into(),
        length: args.length.unwrap_or_default(),
        depth: args.depth.unwrap_or_default(),
    };

    let _pager = Pager::new();

    // Shared state for oneline mode: buffer the current entry's revision_number and message
    let oneline_state = OnelineState::default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileHistory(data) => {
                if oneline {
                    oneline_state.buffer_entry(data.revision_number);
                } else {
                    // Print newline for all but the first entry
                    if first_entry.load(Ordering::Relaxed) {
                        first_entry.store(false, Ordering::Relaxed);
                    } else {
                        println!();
                    }

                    print!(
                        "{}{}",
                        FileActionStyle::from_action(data.action),
                        data.action.as_string_short()
                    );

                    if !data.parent[1].is_zero() {
                        print!(" (MERGE)");
                    }

                    if data.action == LoreFileAction::Add
                        || data.action == LoreFileAction::Move
                        || data.action == LoreFileAction::Copy
                    {
                        print!(" {}{}", CommonStyles::HEADERS, data.path.as_str());
                    }

                    println!("{}", anstyle::Reset);

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
                    if !data.parent[1].is_zero() {
                        println!(
                            "{}Merge     :{} {}",
                            CommonStyles::HEADERS,
                            anstyle::Reset,
                            data.parent[1]
                        );
                    }
                    println!(
                        "{}Address   :{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.address
                    );
                }
            }
            LoreEvent::Metadata(data) => {
                if oneline {
                    if data.key.as_str() == metadata::MESSAGE
                        && let LoreMetadata::String(value) = &data.value
                        && let Some(first_line) = value.as_str().lines().next()
                    {
                        oneline_state.set_message(first_line.to_string());
                    }
                } else {
                    print_metadata(data, None, None);
                }
            }
            LoreEvent::Complete(_) | LoreEvent::Error(_) if oneline => {
                oneline_state.flush();
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(file::history(globals, log_args, callback)) as u8;
}

pub fn handle_file_write(globals: LoreGlobalArgs, args: &FileWriteArgs) -> u8 {
    let write_args = LoreFileWriteArgs {
        address: LoreString::from(&args.address),
        revision: LoreString::from(&args.revision),
        path: LoreString::from(&args.path),
        output: LoreString::from(&args.output),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileWrite(data) => {
                println!(
                    "{}File written {}{}",
                    CommonStyles::SUCCESS,
                    data.path,
                    anstyle::Reset
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Error(_data) => {
                println!(
                    "{}Failed to write file{}",
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

    return runtime().block_on(file::write(globals, write_args, callback)) as u8;
}

pub fn handle_file_obliterate(globals: LoreGlobalArgs, args: &FileObliterateArgs) -> u8 {
    let obliterate_args = LoreFileObliterateArgs {
        address: LoreString::from(&args.address),
        path: LoreString::from(&args.path),
    };

    let file_path = args.path.clone().unwrap_or_default();

    let _spinner = ProgressBar::new_spinner("Obliterating files...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileObliterate(data) => {
                if !file_path.is_empty() {
                    println!("Obliterating file: {file_path}");
                } else {
                    println!("Obliterating address: {}", data.address);
                }
                println!(
                    "Obliterated {} fragments and removed {} payloads",
                    data.num_fragments, data.num_payloads
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Error(_data) => {
                println!(
                    "{}Failed to obliterate file{}",
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

    return runtime().block_on(file::obliterate(globals, obliterate_args, callback)) as u8;
}

pub fn handle_file_dump(globals: LoreGlobalArgs, args: &FileDumpArgs) -> u8 {
    let dump_args = LoreFileDumpArgs {
        address: LoreString::from(&args.address),
        path: LoreString::from(&args.path),
    };

    let file_path = args.path.clone().unwrap_or_default();

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileDump(data) => {
                if !file_path.is_empty() {
                    println!(
                        "{}Path        :{} {file_path}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                }
                println!(
                    "{}Address     :{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.address
                );
                println!(
                    "{}Flags       :{} {:x}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.flags
                );
                println!(
                    "{}Payload size:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.size_payload
                );
                println!(
                    "{}Content size:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.size_content
                );
                println!(
                    "{}Store match :{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.match_made
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Error(_data) => {
                println!(
                    "{}Failed to dump file{}",
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

    return runtime().block_on(file::dump(globals, dump_args, callback)) as u8;
}

pub fn handle_file_metadata_commands(cmd: &FileMetadataCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        FileMetadataCommands::Clear(args) => {
            return handle_file_metadata_clear(globals, args);
        }
        FileMetadataCommands::Get(args) => {
            return handle_file_metadata_get(globals, args);
        }
        FileMetadataCommands::Set(args) => {
            return handle_file_metadata_set(globals, args);
        }
    }
}

pub fn handle_file_dependency_add(globals: LoreGlobalArgs, args: &FileDependencyAddArgs) -> u8 {
    let dep_paths: Vec<LoreString> = args
        .dependencies
        .iter()
        .map(|d| LoreString::from(d.as_str()))
        .collect();
    let tags: Vec<LoreString> = args
        .tags
        .as_ref()
        .map(|t| t.iter().map(|s| LoreString::from(s.as_str())).collect())
        .unwrap_or_default();

    let add_args = dependency::LoreFileDependencyAddArgs {
        paths: LoreArray::from_vec(vec![LoreString::from(args.source.as_str())]),
        dependencies: LoreArray::from_vec(dep_paths),
        tags: LoreArray::from_vec(tags),
        dep_counts: LoreArray::from_vec(vec![args.dependencies.len() as u32]),
        tag_counts: LoreArray::default(),
        force: args.force as u8,
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileDependencyAddEntry(data) => {
                let tags_display = format_tag_list(&data.tags);
                println!(
                    "  {} -> {}{}",
                    data.path.as_str(),
                    data.dependency.as_str(),
                    tags_display
                );
            }
            LoreEvent::FileDependencyAddEnd(data) => {
                if data.added_count > 0 {
                    println!("Added {} dependency edge(s)", data.added_count);
                } else {
                    println!("No new edges (already existed)");
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(dependency::dependency_add(globals, add_args, callback)) as u8
}

pub fn handle_file_dependency_remove(
    globals: LoreGlobalArgs,
    args: &FileDependencyRemoveArgs,
) -> u8 {
    let dep_paths: Vec<LoreString> = args
        .dependencies
        .iter()
        .map(|d| LoreString::from(d.as_str()))
        .collect();
    let tags: Vec<LoreString> = args
        .tags
        .as_ref()
        .map(|t| t.iter().map(|s| LoreString::from(s.as_str())).collect())
        .unwrap_or_default();

    let remove_args = dependency::LoreFileDependencyRemoveArgs {
        paths: LoreArray::from_vec(vec![LoreString::from(args.source.as_str())]),
        dependencies: LoreArray::from_vec(dep_paths),
        tags: LoreArray::from_vec(tags),
        dep_counts: LoreArray::from_vec(vec![args.dependencies.len() as u32]),
        tag_counts: LoreArray::default(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileDependencyRemoveEntry(data) => {
                let tags_display = format_tag_list(&data.tags);
                println!(
                    "  {} -x {}{}",
                    data.path.as_str(),
                    data.dependency.as_str(),
                    tags_display
                );
            }
            LoreEvent::FileDependencyRemoveEnd(data) => {
                println!("Removed {} dependency edge(s)", data.removed_count);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(dependency::dependency_remove(
        globals,
        remove_args,
        callback,
    )) as u8
}

pub fn handle_file_dependency_list(globals: LoreGlobalArgs, args: &FileDependencyListArgs) -> u8 {
    let paths: Vec<LoreString> = args
        .paths
        .as_ref()
        .map(|p| p.iter().map(|s| LoreString::from(s.as_str())).collect())
        .unwrap_or_default();
    let tags: Vec<LoreString> = args
        .tags
        .as_ref()
        .map(|t| t.iter().map(|s| LoreString::from(s.as_str())).collect())
        .unwrap_or_default();

    let list_args = dependency::LoreFileDependencyListArgs {
        paths: LoreArray::from_vec(paths),
        revision: LoreString::from(&args.revision),
        recursive: args.recursive as u8,
        reverse: args.reverse as u8,
        tags: LoreArray::from_vec(tags),
        depth_limit: args.depth,
    };

    let reverse = args.reverse;

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::FileDependencyListFile(data) => {
                if reverse {
                    println!("{} (depended on by):", data.path.as_str());
                } else {
                    println!("{}:", data.path.as_str());
                }
            }
            LoreEvent::FileDependencyListEntry(data) => {
                let tags_display = format_tag_list(&data.tags);
                let indent = "  ".repeat(data.depth.max(1) as usize);
                println!("{}{}{}", indent, data.path.as_str(), tags_display);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    runtime().block_on(dependency::dependency_list(globals, list_args, callback)) as u8
}

fn format_tag_list(tags: &LoreArray<LoreString>) -> String {
    if tags.as_slice().is_empty() {
        String::new()
    } else {
        let tag_strs: Vec<&str> = tags.as_slice().iter().map(|t| t.as_str()).collect();
        format!(" [{}]", tag_strs.join(", "))
    }
}

pub fn handle_file_dependency_commands(
    cmd: &FileDependencyCommands,
    globals: LoreGlobalArgs,
) -> u8 {
    match cmd {
        FileDependencyCommands::Add(args) => handle_file_dependency_add(globals, args),
        FileDependencyCommands::Remove(args) => handle_file_dependency_remove(globals, args),
        FileDependencyCommands::List(args) => handle_file_dependency_list(globals, args),
    }
}

pub fn handle_file_commands(cmd: &FileCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        FileCommands::Info(args) => {
            return handle_file_info(globals, args);
        }
        FileCommands::Diff(args) => {
            return handle_file_diff(globals, args);
        }
        FileCommands::Hash(args) => {
            return handle_file_hash(globals, args);
        }
        FileCommands::Metadata(sub_cmd) => {
            return handle_file_metadata_commands(&sub_cmd.command, globals);
        }
        FileCommands::Dependency(sub_cmd) => {
            return handle_file_dependency_commands(&sub_cmd.command, globals);
        }
        FileCommands::Stage(args) => {
            return handle_file_stage(globals, args);
        }
        FileCommands::Dirty(args) => {
            return handle_file_dirty(globals, args);
        }
        FileCommands::Unstage(args) => {
            return handle_file_unstage(globals, args);
        }
        FileCommands::Write(args) => {
            return handle_file_write(globals, args);
        }
        FileCommands::Reset(args) => {
            return handle_file_reset(globals, args);
        }
        FileCommands::History(args) => {
            return handle_file_history(globals, args);
        }
        FileCommands::Obliterate(args) => {
            return handle_file_obliterate(globals, args);
        }
        FileCommands::Dump(args) => {
            return handle_file_dump(globals, args);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_context_lines;

    #[test]
    fn rejects_non_numeric() {
        assert_eq!(
            parse_context_lines("abc").unwrap_err(),
            "expected a non-negative integer; got 'abc'"
        );
    }

    #[test]
    fn rejects_negative() {
        assert_eq!(
            parse_context_lines("-1").unwrap_err(),
            "expected a non-negative integer; got '-1'"
        );
    }

    #[test]
    fn accepts_valid() {
        assert_eq!(parse_context_lines("5").unwrap(), 5);
    }

    /// Verify `-1` reaches the value parser (rather than being rejected as an
    /// unexpected argument), which is the behavior `allow_negative_numbers` enables.
    #[test]
    fn diff_negative_context_reaches_parser() {
        use clap::Parser;
        let result =
            crate::cli::LoreCli::try_parse_from(["lore", "diff", "-U", "-1", "original.txt"]);
        let err = result
            .err()
            .expect("expected -U -1 to be rejected")
            .to_string();
        assert!(err.contains("non-negative integer"), "got: {err}");
    }
}
