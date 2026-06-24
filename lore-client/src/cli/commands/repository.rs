// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use clap::Args;
use clap::Subcommand;
use lore::interface::LoreArray;
use lore::interface::LoreEvent;
use lore::interface::LoreFileAction;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreMetadataType;
use lore::interface::LoreNodeType;
use lore::interface::LoreRepositoryCloneArgs;
use lore::interface::LoreRepositoryCreateArgs;
use lore::interface::LoreRepositoryDumpArgs;
use lore::interface::LoreRepositoryGcArgs;
use lore::interface::LoreRepositoryInfoArgs;
use lore::interface::LoreRepositoryListArgs;
use lore::interface::LoreRepositoryMetadataClearArgs;
use lore::interface::LoreRepositoryMetadataGetArgs;
use lore::interface::LoreRepositoryMetadataSetArgs;
use lore::interface::LoreRepositoryStatusArgs;
use lore::interface::LoreRepositoryStoreImmutableQueryArgs;
use lore::interface::LoreRepositoryVerifyFragmentArgs;
use lore::interface::LoreRepositoryVerifyStateArgs;
use lore::interface::LoreString;
use lore::repository;
use lore::repository::LoreRepositoryDeleteArgs;
use lore::runtime;
use parking_lot::Mutex;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::println;
use crate::progress_bar::ProgressBar;
use crate::styling::BranchStyles;
use crate::styling::CommonStyles;
use crate::styling::FileActionStyle;
use crate::util;
use crate::util::convert_paths_and_targets;
use crate::util::format_bytes_to_string;

#[derive(Args)]
pub struct RepositoryArgs {
    #[command(subcommand)]
    pub command: RepositoryCommands,
}

#[derive(Args)]
pub struct RepositoryStatusArgs {
    /// Walk the filesystem under the given paths and reconcile every
    /// file against the current revision.
    ///
    /// Detected modifications, adds, and deletes are marked dirty;
    /// stale dirty flags are cleared. The refreshed flags are
    /// persisted in the staged state so subsequent `lore stage` and
    /// `lore status` calls see an accurate picture without
    /// rescanning.
    ///
    /// Without `--scan`, status reports only what is currently
    /// tracked: the staged revision (if any) plus files already
    /// marked dirty. Mark files individually with `lore dirty` for
    /// targeted updates, or pass `--scan` here for bulk
    /// reconciliation.
    #[clap(long, action)]
    scan: bool,

    /// Alias for --scan (backward compatibility)
    #[clap(long, action, hide = true)]
    unstaged: bool,

    /// Verify already-dirty files against the filesystem without a full
    /// scan.
    ///
    /// Each file currently marked dirty is re-checked: one whose on-disk
    /// content still matches the tracked revision (same size, and same
    /// content when the modification time differs) has its dirty flag
    /// cleared and is dropped from the report, unless it is also staged.
    /// Adds, moves, copies, and deletes are always reported. The refreshed
    /// flags are persisted, so this requires write access.
    #[clap(long, action)]
    check_dirty: bool,

    /// Drop the existing staged anchor before computing status.
    /// Combine with --scan to scan from a clean slate.
    #[clap(long, action)]
    reset: bool,

    /// Only show revision info, skip all diffs
    #[clap(long, action)]
    revision_only: bool,

    /// Count directories and files (staged state if present, else current revision; view-filtered)
    #[clap(long, action)]
    count: bool,

    /// Optional paths in repository
    path: Option<Vec<String>>,

    /// Path to a targets file
    #[clap(long, value_name = "file")]
    targets: Option<String>,
}

#[derive(Args)]
pub struct RepositoryCreateArgs {
    /// URL of repository
    #[clap(value_name = "url")]
    url: String,

    /// Optional description of repository
    #[clap(long, value_name = "description")]
    description: Option<String>,

    /// Optional ID of repository
    #[clap(long, value_name = "id")]
    id: Option<String>,

    /// Use the shared store rather than create a local immutable store
    #[clap(long)]
    use_shared_store: bool,

    /// Use this path rather than the system default as the shared store location
    #[clap(long, requires = "use_shared_store")]
    shared_store_path: Option<String>,
}

#[derive(Args)]
pub struct RepositoryCloneArgs {
    /// URL of repository
    #[clap(value_name = "url")]
    url: String,

    /// Path to clone into
    #[clap(value_name = "path")]
    path: Option<String>,

    /// Optional client side view filter file
    #[clap(long, value_name = "view")]
    view: Option<String>,

    /// Optional revision to sync
    #[clap(long, value_name = "revision")]
    revision: Option<String>,

    /// Optional branch to sync (shorthand for a full revision specifier)
    #[clap(long, value_name = "branch", conflicts_with = "revision")]
    branch: Option<String>,

    /// Clone without files, only fetch latest revision tree
    #[clap(long, action)]
    bare: bool,

    /// Clone virtually using split-write filesystem
    #[clap(long = "virtual", action)]
    virtually: bool,

    /// Write directly to the destination file instead of write to a temporary file and move into place
    #[clap(long, action)]
    direct_file_write: bool,

    /// Use direct file I/O instead of memory mapping files
    #[clap(long, action)]
    direct_file_io: bool,

    /// Layer to add
    #[clap(long, value_name = "repository")]
    layer: Option<String>,

    /// Metadata key to link layer revisions with
    #[clap(long, value_name = "key")]
    layer_metadata: Option<String>,

    /// File containing list of files to prefetch
    #[clap(long, value_name = "file")]
    prefetch: Option<String>,

    /// Use the shared store rather than create a local immutable store
    #[clap(long)]
    use_shared_store: bool,

    /// Use this path rather than the system default as the shared store location
    #[clap(long, requires = "use_shared_store")]
    shared_store_path: Option<String>,

    /// Clone without local repository tracking (memory-only stores)
    #[clap(long, action)]
    no_tracking: bool,

    /// Root files for dependency-based selective clone (only clone these files and their dependencies)
    #[clap(long = "root-file", value_name = "path")]
    root_files: Vec<String>,

    /// Tags to filter dependencies by during dependency-based clone
    #[clap(long = "dependency-tag", value_name = "tag")]
    dependency_tags: Vec<String>,

    /// Follow transitive dependencies recursively during dependency-based clone
    #[clap(long, action)]
    dependency_recursive: bool,

    /// Maximum dependency traversal depth (0 means unlimited)
    #[clap(long, value_name = "depth", default_value = "0")]
    dependency_depth_limit: u32,
}

#[derive(Args)]
pub struct RepositoryDeleteArgs {
    /// URL of repository
    #[clap(value_name = "url")]
    url: String,
}

#[derive(Args)]
pub struct RepositoryInfoArgs {
    /// URL of repository
    #[clap(value_name = "url")]
    url: Option<String>,
}

#[derive(Args)]
pub struct RepositoryDumpArgs {
    /// Optional path in the repository to start dumping from
    #[clap(long, value_name = "path")]
    path: Option<String>,

    /// Optional revision to dump
    #[clap(long, value_name = "revision")]
    revision: Option<String>,

    /// Optional max depth of tree dump
    #[clap(long, value_name = "max-depth")]
    max_depth: Option<usize>,
}

#[derive(Args)]
pub struct RepositoryListArgs {
    /// URL of remote
    #[clap(value_name = "url")]
    url: String,
}

#[derive(Args)]
pub struct RepositoryStoreArgs {
    /// Store action
    #[command(subcommand)]
    subcommand: RepositoryStoreCommands,
}

#[derive(Args)]
pub struct RepositoryStoreImmutableArgs {
    /// Store action
    #[command(subcommand)]
    subcommand: RepositoryStoreImmutableCommands,
}

#[derive(Args)]
pub struct RepositoryStoreImmutableQueryArgs {
    /// Fragment address to query
    address: String,

    /// Recurse into subfragments
    #[clap(long, action)]
    recurse: bool,
}

#[derive(Args)]
pub struct RepositoryVerifyArgs {
    #[command(subcommand)]
    pub command: Option<RepositoryVerifyCommands>,

    /// Optional path in the repository to start verification from (for state verification)
    #[clap(long, value_name = "path")]
    path: Option<String>,

    /// Attempt to heal discrepancies found in a new staged state
    #[clap(long, action)]
    heal: bool,
}

#[derive(Subcommand)]
pub enum RepositoryVerifyCommands {
    /// Verify repository state consistency (default)
    State(RepositoryVerifyStateArgs),
    /// Verify a specific fragment in the local store
    Fragment(RepositoryVerifyFragmentArgs),
}

#[derive(Args)]
pub struct RepositoryVerifyStateArgs {
    /// Optional path in the repository to start verification from
    #[clap(long, value_name = "path")]
    path: Option<String>,

    /// Attempt to heal discrepancies found in a new staged state
    #[clap(long, action)]
    heal: bool,
}

#[derive(Args)]
pub struct RepositoryVerifyFragmentArgs {
    /// Fragment hash to verify
    hash: String,

    /// Context part of the address to verify
    #[clap(long)]
    context: Option<String>,

    /// Attempt to heal if verification fails (remote only)
    #[clap(long, action)]
    heal: bool,
}

#[derive(Subcommand)]
pub enum RepositoryCommands {
    /// Show current repository status.
    ///
    /// Reports the staged revision (if any) and the files currently
    /// marked dirty. No filesystem walk runs by default — pass
    /// `--scan` to walk the filesystem and refresh dirty flags. See
    /// `lore status --help` (top-level alias) for the full workflow.
    Status(RepositoryStatusArgs),

    /// Get info about a repository
    Info(RepositoryInfoArgs),

    /// List repositories
    List(RepositoryListArgs),

    // TODO(vri): Add optional path arg?
    /// Create a repository in the given directory
    Create(RepositoryCreateArgs),

    /// Clone a remote repository into the given path
    Clone(RepositoryCloneArgs),

    /// Delete a repository
    Delete(RepositoryDeleteArgs),

    /// Verify repository state consistency
    Verify(RepositoryVerifyArgs),

    /// Dump repository state information
    Dump(RepositoryDumpArgs),

    /// Run a full garbage collection pass on the local repository store
    Gc,

    /// Access the repository data store
    Store(RepositoryStoreArgs),

    /// Repository metadata operations
    Metadata(RepositoryMetadataArgs),

    /// Instance management
    Instance(RepositoryInstanceArgs),

    /// Read a configuration value
    #[command(name = "config")]
    Config(RepositoryConfigArgs),

    /// Update the stored path for this instance
    #[command(name = "update-path")]
    UpdatePath,
}

#[derive(Args)]
pub struct RepositoryInstanceArgs {
    #[command(subcommand)]
    pub command: RepositoryInstanceCommands,
}

#[derive(Subcommand)]
pub enum RepositoryInstanceCommands {
    /// List all registered instances for this repository
    List,
    /// Remove stale instance entries
    Prune,
}

#[derive(Args)]
pub struct RepositoryConfigArgs {
    /// Operation to perform
    #[command(subcommand)]
    pub command: RepositoryConfigCommands,
}

#[derive(Subcommand)]
pub enum RepositoryConfigCommands {
    /// Get a configuration value
    Get(RepositoryConfigGetArgs),
}

#[derive(Args)]
pub struct RepositoryConfigGetArgs {
    /// The configuration key to read
    pub key: String,
}

#[derive(Args)]
pub struct RepositoryMetadataArgs {
    #[command(subcommand)]
    pub command: RepositoryMetadataCommands,
}

#[derive(Subcommand)]
pub enum RepositoryMetadataCommands {
    /// Get metadata from the repository (omit key to list all)
    Get(RepositoryMetadataGetArgs),

    /// Set metadata on the repository
    Set(RepositoryMetadataSetArgs),

    /// Clear metadata from the repository
    Clear(RepositoryMetadataClearArgs),
}

#[derive(Args)]
pub struct RepositoryMetadataGetArgs {
    /// Attribute to get (omit to list all)
    #[clap(value_name = "key")]
    key: Option<String>,
}

#[derive(Args)]
pub struct RepositoryMetadataSetArgs {
    /// Metadata key/value pairs
    #[clap(value_name = "pairs", num_args = 1..)]
    pairs: Option<Vec<String>>,

    /// Indicator that values are paths to binary files
    #[clap(long, action)]
    binary: bool,

    /// Indicator that values are numeric (u64)
    #[clap(long, action, conflicts_with = "binary")]
    numeric: bool,
}

#[derive(Args)]
pub struct RepositoryMetadataClearArgs {
    /// Keys to clear (omit to clear all user-defined keys)
    #[clap(value_name = "keys", num_args = 0..)]
    keys: Option<Vec<String>>,
}

#[derive(Subcommand)]
pub enum RepositoryStoreCommands {
    /// Operations on the immutable store
    Immutable(RepositoryStoreImmutableArgs),
}

#[derive(Subcommand)]
pub enum RepositoryStoreImmutableCommands {
    /// Query the store
    Query(RepositoryStoreImmutableQueryArgs),
}

fn path_typed(path: &str, node_type: LoreNodeType) -> String {
    let mut path = path.to_string();
    if node_type == LoreNodeType::Directory || node_type == LoreNodeType::Link {
        path.push('/');
    }
    path
}

/// Atomic holder for the dirty-node summary captured from the status callback,
/// printed after the file sections. `seen` distinguishes "summary emitted with
/// all-zero counts" (scan/check-dirty found nothing) from "no summary event"
/// (plain status).
#[derive(Default)]
struct StatusSummaryCounts {
    seen: AtomicBool,
    adds: AtomicU64,
    deletes: AtomicU64,
    modifies: AtomicU64,
    moves: AtomicU64,
    copies: AtomicU64,
}

pub fn handle_repository_status(globals: LoreGlobalArgs, args: &RepositoryStatusArgs) -> u8 {
    let revision_only = args.revision_only;
    let staged = if revision_only { 0u8 } else { true as u8 };
    let scan = if revision_only {
        0u8
    } else if args.scan || args.unstaged {
        1u8
    } else {
        0u8
    };
    let check_dirty = if revision_only {
        0u8
    } else {
        args.check_dirty as u8
    };
    let reset = if revision_only { 0u8 } else { args.reset as u8 };
    let sync_point = false as u8;

    let paths = convert_paths_and_targets(&args.path, &args.targets);

    let args = LoreRepositoryStatusArgs {
        staged,
        scan,
        check_dirty,
        reset,
        sync_point,
        revision_only: revision_only as u8,
        count: args.count as u8,
        paths,
    };

    let staged: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
    let unmerged: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
    let unstaged: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
    // Stash the dirty-node summary (scan/check-dirty only) so it prints after
    // the file sections rather than mid-stream from the callback.
    let summary: Arc<StatusSummaryCounts> = Arc::new(StatusSummaryCounts::default());
    let staged_path = staged.clone();
    let unmerged_path = unmerged.clone();
    let unstaged_path = unstaged.clone();
    let summary_holder = summary.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryStatusRevision(data) => {
                println!(
                    "{}Repository{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.repository
                );
                println!(
                    "On branch {}{}{} revision {} -> {}",
                    BranchStyles::CURRENT_BRANCH,
                    data.branch_name.as_str(),
                    anstyle::Reset,
                    data.revision_number,
                    data.revision
                );
                if data.remote_available != 0 {
                    if data.remote_authorized == 0 {
                        println!(
                            "Remote reachable but could not read remote revision (not authorized or unavailable)"
                        );
                    } else if data.remote_branch_exist != 0 {
                        println!(
                            "Remote revision {} -> {}",
                            data.revision_remote_number, data.revision_remote
                        );
                    } else {
                        println!("Remote branch does not exist");
                    }
                }
                if data.is_local_ahead > 0 {
                    if data.is_remote_ahead > 0 {
                        println!("Local branch has diverged, synchronize to merge");
                    } else {
                        println!("Local branch is ahead of remote");
                    }
                } else if data.is_remote_ahead > 0 {
                    println!("Local branch is behind remote");
                } else if data.remote_branch_exist != 0 {
                    println!("Local branch in sync with remote");
                }
                if !data.revision_staged.is_zero()
                    && data.revision_staged != data.revision
                    && !data.revision_merged.is_zero()
                {
                    println!("Pending merge, incoming revision {}", data.revision_merged);
                }
            }
            LoreEvent::RepositoryStatusCount(data) => {
                println!(
                    "Repository size: {} directories, {} files",
                    data.directories, data.files
                );
            }
            LoreEvent::RepositoryStatusSummary(data) => {
                summary_holder.adds.store(data.adds, Ordering::Relaxed);
                summary_holder.deletes.store(data.deletes, Ordering::Relaxed);
                summary_holder.modifies.store(data.modifies, Ordering::Relaxed);
                summary_holder.moves.store(data.moves, Ordering::Relaxed);
                summary_holder.copies.store(data.copies, Ordering::Relaxed);
                summary_holder.seen.store(true, Ordering::Relaxed);
            }
            LoreEvent::RepositoryStatusFile(data) => {
                if data.flag_staged != 0 {
                    if data.flag_conflict_unresolved == 0 {
                        staged_path.lock().push(data.clone());
                    } else {
                        unmerged_path.lock().push(data.clone());
                    }
                } else {
                    unstaged_path.lock().push(data.clone());
                }
            }
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

    let repo_root = std::path::absolute(globals.repository_path())
        .unwrap_or_else(|_| std::path::PathBuf::from(globals.repository_path()));

    let result = runtime().block_on(repository::status(globals, args, callback)) as u8;

    let cwd = std::env::current_dir().unwrap_or_else(|_| repo_root.clone());
    let display_path = |path: &str, node_type| {
        path_typed(
            &util::relativize_for_display(&repo_root, &cwd, path),
            node_type,
        )
    };

    let files_staged = staged.lock();
    if !files_staged.is_empty() {
        println!(
            "{}Changes staged for commit:{}",
            CommonStyles::HEADERS,
            anstyle::Reset
        );

        for file in files_staged.iter() {
            let color_code = FileActionStyle::from_action_bg(file.action);
            if file.action == LoreFileAction::Move || file.action == LoreFileAction::Copy {
                println!(
                    "{}{}{} {} -> {} {}",
                    color_code,
                    file.action_as_string_short(),
                    anstyle::Reset,
                    display_path(file.from_path.as_str(), file.r#type),
                    display_path(file.path.as_str(), file.r#type),
                    file.merged_as_string_short(),
                );
            } else {
                println!(
                    "{}{}{} {} {}",
                    color_code,
                    file.action_as_string_short(),
                    anstyle::Reset,
                    display_path(file.path.as_str(), file.r#type),
                    file.merged_as_string_short(),
                );
            }
        }
    }

    let files_unmerged = unmerged.lock();
    if !files_unmerged.is_empty() {
        println!(
            "{}Changes in conflict:{}",
            CommonStyles::HEADERS,
            anstyle::Reset
        );

        for file in files_unmerged.iter() {
            println!(
                "{}{} {} {} {}{}{}{}",
                FileActionStyle::from_action_bg(file.action),
                file.action_as_string_short(),
                anstyle::Reset,
                display_path(file.path.as_str(), file.r#type),
                FileActionStyle::CONFLICT,
                file.merged_as_string_short(),
                file.conflict_as_string_short(),
                anstyle::Reset,
            );
        }
    }

    let files_unstaged = unstaged.lock();
    if !files_unstaged.is_empty() {
        let mut seen_unstaged = false;
        for file in files_unstaged.iter() {
            if file.action == LoreFileAction::Add {
                continue;
            }

            if !seen_unstaged {
                println!(
                    "{}Changes not staged for commit:{}",
                    CommonStyles::HEADERS,
                    anstyle::Reset
                );
                seen_unstaged = true;
            }

            let color_code = FileActionStyle::from_action(file.action);

            if file.action == LoreFileAction::Move || file.action == LoreFileAction::Copy {
                println!(
                    "{}{}{} {} -> {}",
                    color_code,
                    file.action_as_string_short(),
                    anstyle::Reset,
                    display_path(file.from_path.as_str(), file.r#type),
                    display_path(file.path.as_str(), file.r#type),
                );
            } else {
                println!(
                    "{}{}{} {}",
                    color_code,
                    file.action_as_string_short(),
                    anstyle::Reset,
                    display_path(file.path.as_str(), file.r#type),
                );
            }
        }

        let mut seen_untracked = false;
        for file in files_unstaged.iter() {
            if file.action != LoreFileAction::Add {
                continue;
            }

            if !seen_untracked {
                println!(
                    "{}Untracked files:{}",
                    CommonStyles::HEADERS,
                    anstyle::Reset
                );
                seen_untracked = true;
            }

            println!(
                "{}{}{} {}",
                FileActionStyle::from_action(file.action),
                file.action_as_string_short(),
                anstyle::Reset,
                display_path(file.path.as_str(), file.r#type)
            );
        }
    }

    if summary.seen.load(Ordering::Relaxed) {
        let adds = summary.adds.load(Ordering::Relaxed);
        let deletes = summary.deletes.load(Ordering::Relaxed);
        let modifies = summary.modifies.load(Ordering::Relaxed);
        let moves = summary.moves.load(Ordering::Relaxed);
        let copies = summary.copies.load(Ordering::Relaxed);
        let total = adds + deletes + modifies + moves + copies;
        if total == 0 {
            println!("No tracked changes");
        } else {
            let parts = [
                (adds, "added"),
                (modifies, "modified"),
                (deletes, "deleted"),
                (moves, "moved"),
                (copies, "copied"),
            ];
            let detail = parts
                .iter()
                .filter(|(count, _)| *count > 0)
                .map(|(count, label)| format!("{count} {label}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "{}Tracked changes:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                detail
            );
        }
    }

    result
}

pub fn handle_repository_info(globals: LoreGlobalArgs, args: &RepositoryInfoArgs) -> u8 {
    let args = LoreRepositoryInfoArgs {
        repository_url: LoreString::from(&args.url),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::RepositoryData(data) => {
                println!(
                    "{}{}{} ({})",
                    CommonStyles::HEADERS,
                    data.name,
                    anstyle::Reset,
                    data.id
                );
                if !data.description.is_empty() {
                    println!();
                    println!("{}", data.description);
                }
                println!();
                println!(
                    "{}Remote URL:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.remote_url
                );
                println!(
                    "{}Default branch:{} {} ({})",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.default_branch_name,
                    data.default_branch
                );
                println!(
                    "{}Creator:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.creator
                );
                if let Some(created) = DateTime::from_timestamp_millis(data.created as i64)
                    .map(|time| time.to_rfc2822())
                {
                    println!(
                        "{}Created:{} {created}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
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

    return runtime().block_on(repository::info(globals, args, callback)) as u8;
}

pub fn handle_repository_list(globals: LoreGlobalArgs, args: &RepositoryListArgs) -> u8 {
    let list_args = LoreRepositoryListArgs {
        url: LoreString::from(&args.url),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::RepositoryListEntry(entry) => {
                println!("{} ({})", entry.name, entry.id);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(repository::list(globals, list_args, callback)) as u8;
}

pub fn handle_repository_create(globals: LoreGlobalArgs, args: &RepositoryCreateArgs) -> u8 {
    // Check if we have a full URL or just a name
    let url = if !args.url.contains("/") {
        match std::env::var("LORE_REMOTE_URL") {
            Ok(mut url) => {
                url.push('/');
                url.push_str(args.url.as_str());
                url
            }
            // Offline/local create never connects to a remote, so a bare
            // repository name is accepted as-is without a host name.
            Err(_) if globals.offline() || globals.local() => args.url.clone(),
            Err(_) => {
                println!("Repository URL must include a host name");
                return 1;
            }
        }
    } else {
        args.url.clone()
    };

    let args = LoreRepositoryCreateArgs {
        repository_url: url.into(),
        id: LoreString::from(&args.id),
        description: LoreString::from(&args.description),
        use_shared_store: args.use_shared_store as u8,
        shared_store_path: args.shared_store_path.as_ref().into(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryCreate(data) => {
                println!(
                    "Created repository {} in {} with ID {}",
                    data.name,
                    data.path.as_str(),
                    data.id,
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(repository::create(globals, args, callback)) as u8;
}

pub fn handle_repository_delete(globals: LoreGlobalArgs, args: &RepositoryDeleteArgs) -> u8 {
    // Check if we have a full URL or just a name
    let url = if !args.url.contains("/") {
        let Ok(mut url) = std::env::var("LORE_REMOTE_URL") else {
            println!("Repository URL must include a host name");
            return 1;
        };
        url.push('/');
        url.push_str(args.url.as_str());
        url
    } else {
        args.url.clone()
    };
    let repository_url = LoreString::from_str(url.as_str());

    let args = LoreRepositoryDeleteArgs { repository_url };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(data) if data.status == 0 => {
                println!(
                    "{}Repository deleted successfully{}",
                    CommonStyles::SUCCESS,
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

    return runtime().block_on(repository::delete(globals, args, callback)) as u8;
}

fn format_clone_retain_replace(retain: u64, replace: u64) -> String {
    if retain > 0 || replace > 0 {
        format!(" ({retain} retained, {replace} replaced)")
    } else {
        String::default()
    }
}

#[allow(clippy::unnecessary_unwrap)]
pub fn handle_repository_clone(globals: LoreGlobalArgs, args: &RepositoryCloneArgs) -> u8 {
    // Check if we have a full URL or just a name
    let repository_url = if !args.url.contains("/") {
        let Ok(mut url) = std::env::var("LORE_REMOTE_URL") else {
            println!("Repository URL must include a host name");
            return 1;
        };
        url.push('/');
        url.push_str(args.url.as_str());
        url
    } else {
        args.url.clone()
    };
    let repository_url = LoreString::from(repository_url);

    let mut globals = globals;
    if let Some(path) = args.path.as_deref() {
        globals.repository_path = LoreString::from(path);
    } else if let Some((_, name)) = repository_url.as_str().rsplit_once('/') {
        globals.repository_path = LoreString::from(name);
    } else {
        println!("No path given and unable to parse repository URL");
        return 1;
    };

    let revision;
    if let Some(branch) = args.branch.as_ref() {
        revision = LoreString::from(&format!("{branch}@latest"));
    } else {
        revision = args.revision.as_ref().into();
    }

    let clone_args = LoreRepositoryCloneArgs {
        repository_url,
        revision,
        view: args.view.as_ref().into(),
        bare: args.bare.into(),
        virtually: args.virtually.into(),
        direct_file_write: args.direct_file_write.into(),
        direct_file_io: args.direct_file_io.into(),
        layer: args.layer.as_ref().into(),
        layer_metadata: args.layer_metadata.as_ref().into(),
        prefetch: args.prefetch.as_ref().into(),
        use_shared_store: args.use_shared_store as u8,
        shared_store_path: args.shared_store_path.as_ref().into(),
        no_tracking: args.no_tracking.into(),
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
                    "Cloned {}/{} files ({}/{}){}\x1b[K",
                    data.count.file_complete,
                    data.count.file_count,
                    format_bytes_to_string(data.count.bytes_transferred),
                    format_bytes_to_string(data.count.bytes_total),
                    format_clone_retain_replace(data.count.file_retain, data.count.file_replace)
                );
                println!(
                    "Branch {}{}{} revision {}",
                    BranchStyles::CURRENT_BRANCH,
                    data.branch.as_str(),
                    anstyle::Reset,
                    data.revision
                );
                println!("Clone complete in {:.2}s", start.elapsed().as_secs_f32());
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

    return runtime().block_on(repository::clone(globals, clone_args, callback)) as u8;
}

pub fn handle_repository_verify(globals: LoreGlobalArgs, args: &RepositoryVerifyArgs) -> u8 {
    match &args.command {
        Some(RepositoryVerifyCommands::State(state_args)) => {
            handle_repository_verify_state(globals, state_args)
        }
        Some(RepositoryVerifyCommands::Fragment(fragment_args)) => {
            handle_repository_verify_fragment(globals, fragment_args)
        }
        None => {
            // Backward compatibility: no subcommand means state verification
            let state_args = RepositoryVerifyStateArgs {
                path: args.path.clone(),
                heal: args.heal,
            };
            handle_repository_verify_state(globals, &state_args)
        }
    }
}

pub fn handle_repository_verify_state(
    globals: LoreGlobalArgs,
    args: &RepositoryVerifyStateArgs,
) -> u8 {
    let verify_args = LoreRepositoryVerifyStateArgs {
        path: LoreString::from(&args.path),
        heal: args.heal.into(),
    };

    let _spinner = ProgressBar::new_spinner("Verifying repository state...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryVerifyStateBegin(_data) => {}
            LoreEvent::RepositoryVerifyStateEnd(data) => {
                if data.healed_staged_state.is_zero() {
                    println!(
                        "{}Verified repository state integrity{}",
                        CommonStyles::SUCCESS,
                        anstyle::Reset
                    );
                } else {
                    println!(
                        "Serialized new healed staged state as {}",
                        data.healed_staged_state
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

    runtime().block_on(repository::verify_state(globals, verify_args, callback)) as u8
}

pub fn handle_repository_verify_fragment(
    globals: LoreGlobalArgs,
    args: &RepositoryVerifyFragmentArgs,
) -> u8 {
    let verify_args = LoreRepositoryVerifyFragmentArgs {
        hash: LoreString::from(&args.hash),
        context: LoreString::from(&args.context),
        heal: args.heal.into(),
    };

    let _spinner = ProgressBar::new_spinner("Verifying fragment...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryVerifyFragment(data) => {
                println!(
                    "{}Fragment:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.hash
                );
                println!(
                    "{}Location:{} group {}, bucket {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.group_index,
                    data.bucket_index
                );
                println!(
                    "{}Index path:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.index_path
                );
                println!(
                    "{}Entries in bucket:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.entry_count
                );
                println!(
                    "{}Packfile entries checked:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.packfile_entry_count
                );
                println!();
                println!(
                    "{}Matches ({}){}:",
                    CommonStyles::HEADERS,
                    data.match_count,
                    anstyle::Reset
                );
                for (i, m) in data.matches.as_slice().iter().enumerate() {
                    println!("  [{}] slot={} index={}", i, m.slot, m.index);
                    println!(
                        "      {}repository:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.repository
                    );
                    println!(
                        "      {}address:{} {}:{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.address_hash,
                        m.address_context
                    );
                    println!(
                        "      {}flags:{} 0x{:x}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.flags
                    );
                    println!(
                        "      {}payload:{} {} bytes",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.size_payload
                    );
                    println!(
                        "      {}content:{} {} bytes",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.size_content
                    );
                    println!(
                        "      {}pack:{} file={} offset={}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.pack_file,
                        m.pack_offset
                    );
                    println!(
                        "      {}last_access:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        m.last_access
                    );
                }
                println!();
                if data.error.is_empty() {
                    println!(
                        "Fragment status: {}OK{}",
                        CommonStyles::SUCCESS,
                        anstyle::Reset
                    );
                } else {
                    println!(
                        "Fragment status: {}ERROR{}: {}",
                        CommonStyles::FAILURE,
                        anstyle::Reset,
                        data.error
                    );
                }
            }
            LoreEvent::RepositoryVerifyFragmentRemote(data) => {
                println!("Fragment: {}:{}", data.address_hash, data.address_context);
                if data.error.is_empty() {
                    let is_corrupted = data.corrupted != 0;
                    if is_corrupted {
                        println!(
                            "Fragment status: {}CORRUPTED{}",
                            CommonStyles::FAILURE,
                            anstyle::Reset
                        );
                    } else {
                        println!(
                            "Fragment status: {}OK{}",
                            CommonStyles::SUCCESS,
                            anstyle::Reset
                        );
                    }
                    match data.healed {
                        0 => println!("Healing: Not Attempted"),
                        1 => println!(
                            "Healing: {}Success{}",
                            CommonStyles::SUCCESS,
                            anstyle::Reset
                        ),
                        2 => println!("Healing: {}Failed{}", CommonStyles::FAILURE, anstyle::Reset),
                        _ => println!("Healing: Unknown"),
                    };
                } else {
                    println!(
                        "Fragment status: {}ERROR{}: {}",
                        CommonStyles::FAILURE,
                        anstyle::Reset,
                        data.error
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

    runtime().block_on(repository::verify_fragment(globals, verify_args, callback)) as u8
}

pub fn handle_repository_dump(
    globals: LoreGlobalArgs,
    revision: &str,
    path: &str,
    max_depth: usize,
) -> u8 {
    let dump_args = LoreRepositoryDumpArgs {
        revision: revision.into(),
        path: path.into(),
        max_depth,
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryDumpBegin(data) => {
                println!(
                    "{}Repository{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.repository
                );
                println!(
                    "{}Revision{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.revision
                );
            }
            LoreEvent::RepositoryStateDump(data) => {
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
                println!(
                    "{}Tree:{} hash {} size {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.tree_hash,
                    data.tree_size
                );
            }
            LoreEvent::RepositoryStateDumpNode(data) => {
                println!(
                    "{} id {} parent {} sibling {} mode 0{:o} size {} flags {:x} {}",
                    data.name,
                    data.id,
                    data.parent,
                    data.sibling,
                    data.mode,
                    data.size,
                    data.flags,
                    data.type_data,
                );
            }
            LoreEvent::RepositoryDumpEnd(_dump) => {}
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(repository::dump(globals, dump_args, callback)) as u8;
}

pub fn handle_repository_gc(globals: LoreGlobalArgs) -> u8 {
    let args = LoreRepositoryGcArgs {};

    // Progress events arrive one per evicted bucket / compacted group. Show a single
    // in-place bar with the running total instead of a line per event, then print the
    // final totals once on completion so the result survives the bar being cleared.
    let bar = ProgressBar::new_spinner("Running GC...");
    let evicted = AtomicU64::new(0);
    let reclaimed = AtomicU64::new(0);

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {
                println!(
                    "Garbage collection complete: compaction reclaimed {}, eviction removed {} fragments",
                    format_bytes_to_string(reclaimed.load(Ordering::Relaxed)),
                    evicted.load(Ordering::Relaxed)
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            LoreEvent::CompactionBegin(data) => {
                reclaimed.store(0, Ordering::Relaxed);
                bar.set_message(format!(
                    "Compacting (target {})",
                    format_bytes_to_string(data.target_bytes)
                ));
            }
            LoreEvent::CompactionProgress(data) => {
                let total = reclaimed.fetch_add(data.compacted_bytes, Ordering::Relaxed)
                    + data.compacted_bytes;
                bar.set_message(format!(
                    "Compacting: {} reclaimed",
                    format_bytes_to_string(total)
                ));
            }
            LoreEvent::CompactionEnd(data) => {
                reclaimed.store(data.total_compacted_bytes, Ordering::Relaxed);
            }
            LoreEvent::EvictionBegin(data) => {
                evicted.store(0, Ordering::Relaxed);
                bar.set_message(format!(
                    "Evicting (target {} fragments)",
                    data.target_fragments
                ));
            }
            LoreEvent::EvictionProgress(data) => {
                let total = evicted.fetch_add(data.evicted, Ordering::Relaxed) + data.evicted;
                bar.set_message(format!("Evicting: {total} fragments"));
            }
            LoreEvent::EvictionEnd(data) => {
                evicted.store(data.total_evicted, Ordering::Relaxed);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(repository::gc(globals, args, callback)) as u8;
}

pub fn handle_repository_store(globals: LoreGlobalArgs, args: &RepositoryStoreArgs) -> u8 {
    match &args.subcommand {
        RepositoryStoreCommands::Immutable(args) => {
            handle_repository_store_immutable(globals, args)
        }
    }
}

pub fn handle_repository_store_immutable(
    globals: LoreGlobalArgs,
    args: &RepositoryStoreImmutableArgs,
) -> u8 {
    match &args.subcommand {
        RepositoryStoreImmutableCommands::Query(args) => {
            handle_repository_store_immutable_query(globals, args)
        }
    }
}

pub fn handle_repository_store_immutable_query(
    globals: LoreGlobalArgs,
    args: &RepositoryStoreImmutableQueryArgs,
) -> u8 {
    let query_args = LoreRepositoryStoreImmutableQueryArgs {
        address: LoreString::from(&args.address),
        recurse: args.recurse.into(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryStoreImmutableQuery(data) => {
                println!(
                    "{}Address{} {}{}{}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.address,
                    if data.remote != 0 {
                        " (remote)"
                    } else {
                        " (local)"
                    },
                    if data.subfragment != 0 {
                        " (subfragment)"
                    } else {
                        ""
                    }
                );
                println!(
                    "{}Status:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    match data.status {
                        0 =>
                            if data.payload != 0 {
                                "Stored (metadata and payload)"
                            } else {
                                "Stored (metadata)"
                            },
                        1 => "Hash exist",
                        2 => "Hash exist in other repository",
                        3 => "Not found",
                        _ => "Unknown",
                    }
                );
                if data.status != 3 {
                    println!(
                        "{}Payload:{} {} bytes{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.payload_size,
                        if data.payload != 0 { " (cached)" } else { "" }
                    );
                    println!(
                        "{}Content:{} {} bytes",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.content_size
                    );
                    println!(
                        "{}Flags:{} 0x{:x}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        data.flags
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

    runtime().block_on(repository::store_immutable_query(
        globals, query_args, callback,
    )) as u8
}

pub fn handle_repository_metadata_get(
    globals: LoreGlobalArgs,
    args: &RepositoryMetadataGetArgs,
) -> u8 {
    let get_args = LoreRepositoryMetadataGetArgs {
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

    runtime().block_on(repository::metadata_get(globals, get_args, callback)) as u8
}

pub fn handle_repository_metadata_set(
    globals: LoreGlobalArgs,
    args: &RepositoryMetadataSetArgs,
) -> u8 {
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

    let set_args = LoreRepositoryMetadataSetArgs {
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

    runtime().block_on(repository::metadata_set(globals, set_args, callback)) as u8
}

pub fn handle_repository_metadata_clear(
    globals: LoreGlobalArgs,
    args: &RepositoryMetadataClearArgs,
) -> u8 {
    let keys: Vec<LoreString> = args
        .keys
        .as_ref()
        .map(|k| k.iter().map(|s| LoreString::from(s.as_str())).collect())
        .unwrap_or_default();

    let clear_args = LoreRepositoryMetadataClearArgs {
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

    runtime().block_on(repository::metadata_clear(globals, clear_args, callback)) as u8
}

pub fn handle_repository_metadata_commands(
    cmd: &RepositoryMetadataCommands,
    globals: LoreGlobalArgs,
) -> u8 {
    match cmd {
        RepositoryMetadataCommands::Get(args) => handle_repository_metadata_get(globals, args),
        RepositoryMetadataCommands::Set(args) => handle_repository_metadata_set(globals, args),
        RepositoryMetadataCommands::Clear(args) => handle_repository_metadata_clear(globals, args),
    }
}

pub fn handle_repository_commands(cmd: &RepositoryCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        RepositoryCommands::Status(args) => handle_repository_status(globals, args),
        RepositoryCommands::Info(args) => handle_repository_info(globals, args),
        RepositoryCommands::List(args) => handle_repository_list(globals, args),
        RepositoryCommands::Create(args) => handle_repository_create(globals, args),
        RepositoryCommands::Delete(args) => handle_repository_delete(globals, args),
        RepositoryCommands::Clone(args) => handle_repository_clone(globals, args),
        RepositoryCommands::Verify(args) => handle_repository_verify(globals, args),
        RepositoryCommands::Dump(args) => handle_repository_dump(
            globals,
            args.revision.as_deref().unwrap_or(""),
            args.path.as_deref().unwrap_or(""),
            args.max_depth.unwrap_or_default(),
        ),
        RepositoryCommands::Gc => handle_repository_gc(globals),
        RepositoryCommands::Store(args) => handle_repository_store(globals, args),
        RepositoryCommands::Metadata(args) => {
            handle_repository_metadata_commands(&args.command, globals)
        }
        RepositoryCommands::Instance(args) => match &args.command {
            RepositoryInstanceCommands::List => handle_repository_instance_list(globals),
            RepositoryInstanceCommands::Prune => handle_repository_instance_prune(globals),
        },
        RepositoryCommands::Config(args) => match &args.command {
            RepositoryConfigCommands::Get(get_args) => {
                handle_repository_config_get(globals, &get_args.key)
            }
        },
        RepositoryCommands::UpdatePath => handle_repository_update_path(globals),
    }
}

fn handle_repository_instance_list(globals: LoreGlobalArgs) -> u8 {
    let args = lore::repository::LoreRepositoryInstanceListArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(|event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::RepositoryInstance(data) => {
                let stale = if data.stale != 0 { " (stale)" } else { "" };
                println!(
                    "{} {} {} {}{}",
                    data.instance_id, data.path, data.branch_name, data.revision, stale
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(lore::repository::instance_list(globals, args, callback)) as u8;
}

fn handle_repository_instance_prune(globals: LoreGlobalArgs) -> u8 {
    let args = lore::repository::LoreRepositoryInstancePruneArgs {};

    let pruned_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let pruned_count_clone = pruned_count.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {
                let count = pruned_count_clone.load(std::sync::atomic::Ordering::Relaxed);
                if count > 0 {
                    println!("Pruned {count} stale instance(s)");
                } else {
                    println!("No stale instances found");
                }
            }
            LoreEvent::RepositoryInstance(data) => {
                pruned_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                println!("  Pruned {} {}", data.instance_id, data.path);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(lore::repository::instance_prune(globals, args, callback)) as u8;
}

fn handle_repository_config_get(globals: LoreGlobalArgs, key: &str) -> u8 {
    let args = lore::repository::LoreRepositoryConfigGetArgs {
        key: LoreString::from(key),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(|event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::RepositoryConfigGet(data) => {
                println!("{}", data.value);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(lore::repository::config_get(globals, args, callback)) as u8;
}

fn handle_repository_update_path(globals: LoreGlobalArgs) -> u8 {
    let args = lore::repository::LoreRepositoryUpdatePathArgs {};

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(|event: &LoreEvent| match event {
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(lore::repository::repository_update_path(
        globals, args, callback,
    )) as u8;
}
