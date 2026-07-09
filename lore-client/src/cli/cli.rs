// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use clap::Parser;
use clap::Subcommand;
use lore::LORE_LIBRARY_VERSION;
use lore::interface::LoreEvent;
use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::commands::*;
use crate::config::config;
use crate::eprintln;
use crate::println;
use crate::styling::cli_styles;
use crate::util::get_repository_path;

// TODO(UCS-12558): Cleanup logging args
#[derive(Parser)]
#[command(name = "lore", styles = cli_styles())]
#[clap(about, long_about = None)]
#[clap(version = LORE_LIBRARY_VERSION.as_str())]
pub struct LoreCli {
    #[command(subcommand)]
    pub command: Option<LoreCommands>,

    /// Use given path as repository path
    #[clap(global = true, long, value_name = "path")]
    pub repository: Option<String>,

    /// Set the logging level
    #[clap(global = true, long = "log-level", value_name = "level")]
    pub level: Option<String>,

    /// Enable debug output
    #[clap(global = true, long, short, action)]
    pub debug: bool,

    /// Suppress all output
    #[clap(global = true, hide = true, long, short, action)]
    pub silent: bool,

    /// Time execution of command
    #[clap(global = true, hide = true, long, short, action)]
    pub time: bool,

    /// Force the operation if possible
    #[clap(global = true, long, short, action)]
    pub force: bool,

    /// Dry run mode, only report what would have been changed and perform no changes to local file system
    #[clap(global = true, long, action)]
    pub dry_run: bool,

    /// Enable machine-readable json output
    #[clap(global = true, hide = true, long, short, action)]
    pub json: bool,

    /// Disable pagination
    #[clap(global = true, long, short = 'P', action)]
    pub no_pager: bool,

    /// Force offline mode
    #[clap(global = true, long, action)]
    pub offline: bool,

    /// Use remote data
    #[clap(global = true, long, action)]
    pub remote: bool,

    /// Use local data
    #[clap(global = true, long, action, conflicts_with = "remote")]
    pub local: bool,

    /// Use given identity
    #[clap(global = true, long, action)]
    pub identity: Option<String>,

    /// Avoid using compression
    #[clap(global = true, hide = true, long, action)]
    pub nocompress: bool,

    /// Set maximum number of parallel connections
    #[clap(global = true, long)]
    pub max_connections: Option<u32>,

    /// Set maximum number of parallel files opened
    #[clap(global = true, long, value_name = "count")]
    pub file_count_limit: Option<usize>,

    /// Set maximum total size in bytes of parallel files opened
    #[clap(global = true, long, value_name = "size")]
    pub file_size_limit: Option<usize>,

    /// Set maximum number of parallel compress operations
    #[clap(global = true, long, value_name = "count")]
    pub compress_limit: Option<usize>,

    /// Set maximum total number of threads Lore sizes its pools for
    #[clap(global = true, hide = true, long, value_name = "count")]
    pub max_threads: Option<usize>,

    #[arg(long, hide = true)]
    pub markdown_help: bool,

    /// Set maximum number of revisions to search when matching or finding revisions
    #[clap(global = true, long)]
    search_limit: Option<usize>,

    /// Set to search for nearest match when matching revisions
    #[clap(global = true, long, action)]
    search_nearest: bool,

    /// Prevent automatic incremental garbage collection for this command; it otherwise runs in the background on writes. `lore repository gc` always runs a full pass regardless
    #[clap(global = true, long = "no-gc", action)]
    no_gc: bool,

    /// Force sync data to storage media during flush
    #[clap(global = true, long, action)]
    sync_data: bool,

    /// Cache fragment payloads fetched from remote in the local store
    #[clap(global = true, long, action)]
    pub cache: bool,

    /// Disable interactive prompts (e.g., per-link commit messages)
    #[clap(global = true, long, action)]
    pub non_interactive: bool,
}

pub type EventCallbackFn = Box<dyn Fn(&LoreEvent) + Send + Sync>;

pub trait EventCallbackExt {
    /// Wraps the callback with default handling for `LoreEvent::Log`.
    /// `LoreEvent::Error` is swallowed — a failing completion routes its
    /// message through `LoreEvent::Log(level=Error)`.
    fn with_defaults(self) -> Self;
}

impl EventCallbackExt for EventCallbackFn {
    fn with_defaults(self) -> Self {
        Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Error(_) => {}
            LoreEvent::Log(data) => {
                crate::logging::handle_log_event(data);
            }
            other => self(other),
        })
    }
}

pub fn output_formatter() -> Option<LoreEventCallback> {
    if config().json {
        Some(Some(Box::new(move |event: &LoreEvent| {
            // Filter log events since we receive all log events for log file,
            // but output should adhere to the configured log level limit
            if let LoreEvent::Log(data) = event
                && data.level < config().log_level
            {
                return;
            }
            // Ignore the end of events event
            if let LoreEvent::End(_) = event {
                return;
            }

            #[derive(Serialize, Deserialize)]
            struct JsonError {
                error: String,
            }

            match serde_json::to_string(event) {
                Ok(string) => println!("{string}"),
                Err(err) => {
                    eprintln!(
                        "{}",
                        serde_json::to_string(&JsonError {
                            error: err.to_string()
                        })
                        .unwrap()
                    );
                }
            }
        }) as Box<_>))
    } else {
        None
    }
}

#[derive(Subcommand)]
pub enum LoreCommands {
    /// Repository commands
    Repository(repository::RepositoryArgs),

    /// Branch commands
    Branch(branch::BranchArgs),

    /// Revision commands
    Revision(revision::RevisionArgs),

    /// File commands
    File(file::FileArgs),

    /// Authentication commands
    Auth(auth::AuthArgs),

    /// Layer commands
    Layer(layer::LayerArgs),

    /// Logfile commands
    Logfile(logfile::LogfileArgs),

    /// Authenticate the CLI
    Login(auth::AuthLoginArgs),

    /// Link commands
    Link(link::LinkArgs),

    // Config commands
    /// Show current repository status.
    ///
    /// Reports the staged revision (if any) plus the files and
    /// directories currently marked dirty. By default no filesystem
    /// walk is performed — only the tracked dirty flags are read, so
    /// changes made without prior `lore dirty` or `--scan` will not
    /// appear.
    ///
    /// Pass `--scan` to walk the filesystem under the given paths,
    /// reconcile every file against the current revision, and refresh
    /// dirty flags (setting them on detected modifications/adds/deletes
    /// and clearing stale ones). The refreshed flags are persisted so
    /// subsequent `lore stage` / `lore status` calls see an accurate
    /// picture without rescanning.
    Status(repository::RepositoryStatusArgs),

    /// Clone a remote repository into the given path
    Clone(repository::RepositoryCloneArgs),

    /// Stage changes for commit.
    ///
    /// Directory path (including `.`): stages only files already marked
    /// dirty under that directory. No filesystem walk is performed;
    /// clean or unmarked files are skipped. Mark files first with
    /// `lore dirty` (or `lore status --scan` to reconcile in bulk), or
    /// pass `--scan` here to discover and stage in one pass.
    ///
    /// Specific file path: checked against the filesystem and staged
    /// if its on-disk content differs from the current revision,
    /// regardless of its dirty flag.
    ///
    /// `--scan`: forces a filesystem walk under the given paths, marks
    /// modified, added, and deleted files dirty, and stages them in
    /// one step. Use this when changes were made externally without
    /// going through `lore dirty`, or to recover after losing track of
    /// dirty state.
    Stage(file::FileStageArgs),
    /// Mark files as dirty so they show up in `lore status` and get
    /// picked up by `lore stage` (no content is read or staged).
    ///
    /// Use this when your editor or build tool has modified files and
    /// you want to inform Lore of the change without performing a full
    /// `--scan`. For bulk reconciliation across a tree, prefer
    /// `lore status --scan` or `lore stage --scan`.
    Dirty(file::FileDirtyArgs),
    /// Unstage changes to a file or directory
    Unstage(file::FileUnstageArgs),
    /// Reset changes to a file or directory
    Reset(file::FileResetArgs),
    /// Show differences between two revisions of a file
    Diff(file::FileDiffArgs),

    /// List revisions of a repository
    History(revision::RevisionHistoryArgs),

    /// Commit the staged revision
    Commit(revision::RevisionCommitArgs),

    /// Synchronize to a repository state
    #[clap(visible_alias("synchronize"))]
    Sync(revision::RevisionSyncArgs),

    /// Push commits to remote
    Push(branch::BranchPushArgs),

    /// Lock file
    Lock(lock::LockFileArgs),

    /// Manage the repository in a service process
    Service(service::ServiceArgs),

    /// Notifications
    Notification(notification::NotificationArgs),

    /// Generate terminal autocompletions
    Completions(completions::CompletionsArgs),

    /// Manage the shared store
    SharedStore(shared_store::SharedStoreArgs),
}

pub fn handle_lore_commands(cmd: &LoreCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        LoreCommands::Repository(sub_cmd) => {
            repository::handle_repository_commands(&sub_cmd.command, globals)
        }
        LoreCommands::Status(args) => repository::handle_repository_status(globals, args),
        LoreCommands::Clone(args) => repository::handle_repository_clone(globals, args),
        LoreCommands::Branch(sub_cmd) => branch::handle_branch_commands(&sub_cmd.command, globals),
        LoreCommands::Revision(sub_cmd) => {
            revision::handle_revision_commands(&sub_cmd.command, globals)
        }
        LoreCommands::File(sub_cmd) => file::handle_file_commands(&sub_cmd.command, globals),
        LoreCommands::Stage(args) => file::handle_file_stage(globals, args),
        LoreCommands::Dirty(args) => file::handle_file_dirty(globals, args),
        LoreCommands::Unstage(args) => file::handle_file_unstage(globals, args),
        LoreCommands::Reset(args) => file::handle_file_reset(globals, args),
        LoreCommands::Diff(args) => file::handle_file_diff(globals, args),
        LoreCommands::History(args) => revision::handle_revision_history(globals, args),
        LoreCommands::Commit(args) => revision::handle_revision_commit(globals, args),
        LoreCommands::Sync(args) => revision::handle_revision_sync(globals, args),
        LoreCommands::Push(args) => branch::handle_branch_push(globals, args),
        LoreCommands::Auth(sub_cmd) => auth::handle_auth_commands(&sub_cmd.command, globals),
        LoreCommands::Layer(sub_cmd) => layer::handle_layer_commands(&sub_cmd.command, globals),
        LoreCommands::Login(args) => auth::handle_login_command(globals, args),
        LoreCommands::Logfile(sub_cmd) => logfile::handle_logfile_commands(&sub_cmd.command),
        LoreCommands::Lock(sub_cmd) => lock::handle_lock_file_commands(globals, &sub_cmd.command),
        LoreCommands::Link(sub_cmd) => link::handle_link_commands(&sub_cmd.command, globals),
        LoreCommands::Service(sub_cmd) => {
            service::handle_service_commands(&sub_cmd.command, globals)
        }
        LoreCommands::Notification(sub_cmd) => {
            notification::handle_notification_commands(&sub_cmd.command, globals)
        }
        LoreCommands::Completions(sub_cmd) => completions::handle_completions_commands(sub_cmd),
        LoreCommands::SharedStore(sub_cmd) => {
            shared_store::handle_store_commands(&sub_cmd.command, globals)
        }
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum LoreCliError {
    #[error("Log level '{0}' is not valid. Choose one of [trace, debug, info, warn, error].")]
    ParseLogLevel(String),
}

pub fn lore_globals_from_args(cli: &LoreCli) -> LoreGlobalArgs {
    let mut args = LoreGlobalArgs {
        repository_path: get_repository_path(cli.repository.clone()),

        force: cli.force.into(),
        dry_run: cli.dry_run.into(),
        offline: cli.offline.into(),
        remote: cli.remote.into(),
        local: cli.local.into(),
        no_gc: cli.no_gc.into(),
        sync_data: cli.sync_data.into(),
        cache: cli.cache.into(),

        identity: cli.identity.clone().into(),

        search_limit: cli.search_limit.unwrap_or_default() as u32,
        search_nearest: cli.search_nearest.into(),

        ..Default::default()
    };

    if let Some(limit) = cli.file_count_limit
        && limit > 0
    {
        args.file_count_limit = limit as u64;
    }

    if let Some(limit) = cli.file_size_limit
        && limit > 0
    {
        args.file_size_limit = limit as u64;
    }

    if let Some(limit) = cli.compress_limit
        && limit > 0
    {
        args.compress_task_limit = limit as u64;
    }

    args.max_connections = if let Some(max_connections) = cli.max_connections {
        max_connections
    } else if let Ok(max_connections) = std::env::var("LORE_MAX_CONNECTIONS")
        && let Ok(max_connections) = str::parse(max_connections.as_str())
    {
        max_connections
    } else {
        0
    };

    args
}

// TODO(vri): Add command shortcuts
