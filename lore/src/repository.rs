// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime_flush_guarded;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::global::GlobalConfig;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreMetadataType;
use lore_revision::lore::RepositoryId;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::SharedStoreToUseConfig;
use lore_revision::repository::clone::CloneError;
use lore_revision::repository::clone::CloneLayer;
use lore_revision::repository::clone::CloneOptions;
use lore_revision::repository::create::CreateError;
use lore_revision::repository::create::CreateMetadata;
use lore_revision::repository::create::CreateOptions;
use lore_revision::repository::status::StatusOptions;
use lore_revision::revision;
use lore_revision::util;
use lore_revision::util::path::RelativePath;
use serde::Deserialize;
use serde::Serialize;

use crate::call::no_repository_call;
use crate::call::repository_call_no_store;
use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call::setup_execution;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreString;
use crate::util::convert_user_paths;
use crate::util::log_command_done;
use crate::util::log_command_info;

/// Arguments for cloning a remote repository to the local path.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(clone_local)]
pub struct LoreRepositoryCloneArgs {
    /// URL to the repository
    pub repository_url: LoreString,
    /// [Optional] Revision to clone
    pub revision: LoreString,
    /// [Optional] Client side view filter to use
    pub view: LoreString,
    /// Clone without any files
    pub bare: u8,
    /// Clone virtually using split-write filesystem
    pub virtually: u8,
    /// Use direct file write
    pub direct_file_write: u8,
    /// Use direct file I/O instead of memory mapping files
    pub direct_file_io: u8,
    /// (Optional) Layer module
    pub layer: LoreString,
    /// (Optional) Layer metadata key to link revisions with
    pub layer_metadata: LoreString,
    /// (Optional) File containing list of files to prefetch
    pub prefetch: LoreString,
    /// Use the shared store instead of a local immutable store
    pub use_shared_store: u8,
    /// [Optional] Path to use for the shared store, an empty string means to use the default
    pub shared_store_path: LoreString,
    /// Clone without local repository tracking (memory-only stores)
    pub no_tracking: u8,
    /// Root files for dependency-based selective clone
    pub root_files: LoreArray<LoreString>,
    /// Tags to filter dependencies by during resolution
    pub dependency_tags: LoreArray<LoreString>,
    /// Follow transitive dependencies recursively
    pub dependency_recursive: u8,
    /// Maximum dependency traversal depth. 0 means unlimited.
    pub dependency_depth_limit: u32,
}

/// Clones a remote repository to the local path specified in the global arguments.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Clone Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryCloneBegin`](crate::interface::LoreEvent::RepositoryCloneBegin) | Emitted when clone begins, includes remote URL and target path |
/// | [`LoreEvent::RepositoryCloneProgress`](crate::interface::LoreEvent::RepositoryCloneProgress) | Emitted periodically during clone with progress data, and at completion with final totals |
/// | [`LoreEvent::RepositoryCloneEnd`](crate::interface::LoreEvent::RepositoryCloneEnd) | Emitted when clone completes successfully |
/// | [`LoreEvent::RevisionSyncTarget`](crate::interface::LoreEvent::RevisionSyncTarget) | Emitted after resolving the target revision to sync during clone |
/// | [`LoreEvent::RevisionSyncRevision`](crate::interface::LoreEvent::RevisionSyncRevision) | Emitted with the resulting revision |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted periodically during initial file sync |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file written during initial sync |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by view filters |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each fragment written to the local store |
pub async fn clone(
    globals: LoreGlobalArgs,
    args: LoreRepositoryCloneArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, clone_local).await
}

async fn clone_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryCloneArgs,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&clone, &args);

            let time_start = Instant::now();

            let result = clone_impl(execution_context().globals().repository_path(), &args).await;

            log_command_done(&clone, time_start);
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

async fn clone_impl(
    repository_path: &str,
    args: &LoreRepositoryCloneArgs,
) -> Result<(), CloneError> {
    let remote_url = args.repository_url.as_str();
    let clone_path = util::path::make_absolute(repository_path)
        .forward_with::<CloneError, _>(|| format!("Invalid path: {repository_path}"))?;
    let bare = args.bare != 0;
    let ignore_existing = false;
    let virtually = args.virtually != 0;
    let direct_file_write = args.direct_file_write != 0;
    let direct_file_io = args.direct_file_io != 0;
    let no_tracking = args.no_tracking != 0;

    let view_path = if args.view.length > 0 {
        Some(
            util::path::make_absolute(args.view.as_str())
                .forward_with::<CloneError, _>(|| format!("Failed to load view {}", args.view))?,
        )
    } else {
        None
    };
    let prefetch = args.prefetch.clone().into();

    let global_config = GlobalConfig::load()
        .await
        .forward::<CloneError>("Couldn't load global config")?;
    let shared_store_options = SharedStoreToUseConfig::from_cli_args(
        &global_config,
        args.use_shared_store,
        &args.shared_store_path,
    )
    .forward_with::<CloneError, _>(|| format!("Invalid path: {}", args.shared_store_path))?;

    let root_files: Vec<String> = args
        .root_files
        .as_slice()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let dependency_tags: Vec<String> = args
        .dependency_tags
        .as_slice()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let options = CloneOptions {
        bare,
        ignore_existing,
        virtually,
        direct_file_write,
        direct_file_io,
        prefetch,
        shared_store_options,
        no_tracking,
        root_files,
        dependency_tags,
        dependency_recursive: args.dependency_recursive != 0,
        dependency_depth_limit: args.dependency_depth_limit,
    };

    let layer = if !args.layer.is_empty() {
        Some(CloneLayer {
            module: args.layer.to_string(),
            module_path: String::default(),
            layer_path: String::default(),
            metadata: args.layer_metadata.clone().into(),
        })
    } else {
        None
    };

    lore_revision::repository::clone::clone(
        remote_url,
        execution_context().globals().identity().unwrap_or_default(),
        clone_path.as_path(),
        args.revision.clone().into(),
        view_path.as_deref(),
        layer,
        options,
    )
    .await
}

/// Arguments for retrieving metadata about a remote repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(info_local)]
pub struct LoreRepositoryInfoArgs {
    /// URL of the remote repository to query
    pub repository_url: LoreString,
}

/// Retrieves metadata about a remote repository, such as its name, URL, and branch information.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Repository Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryData`](crate::interface::LoreEvent::RepositoryData) | Emitted with repository metadata (name, URL, branch info, etc.) |
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreRepositoryInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_local).await
}

async fn info_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&info, &args);

            let time_start = Instant::now();

            let result = repository::info::info(
                (&args.repository_url).into(),
                execution_context().globals().identity().unwrap_or_default(),
            )
            .await;

            log_command_done(&info, time_start);
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for dumping the internal state tree of the repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(dump_local)]
pub struct LoreRepositoryDumpArgs {
    /// Revision to dump; empty string uses the current revision
    pub revision: LoreString,
    /// Repository-relative path to start dumping from; empty dumps the root
    pub path: LoreString,
    /// Maximum tree traversal depth
    pub max_depth: usize,
}

/// Dumps the internal state tree of the repository for diagnostic purposes.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Repository Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryDumpBegin`](crate::interface::LoreEvent::RepositoryDumpBegin) | Emitted before dump output begins |
/// | [`LoreEvent::RepositoryDumpEnd`](crate::interface::LoreEvent::RepositoryDumpEnd) | Emitted when dump completes |
/// | [`LoreEvent::RepositoryStateDump`](crate::interface::LoreEvent::RepositoryStateDump) | Emitted with repository state summary |
/// | [`LoreEvent::RepositoryStateDumpNode`](crate::interface::LoreEvent::RepositoryStateDumpNode) | Emitted for each node in the state tree |
pub async fn dump(
    globals: LoreGlobalArgs,
    args: LoreRepositoryDumpArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dump_local).await
}

async fn dump_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryDumpArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, dump, dump_impl).await
}

async fn dump_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRepositoryDumpArgs,
) -> Result<(), RepositoryError> {
    // Revision is an optional argument, so check for if it was provided
    let revision = if args.revision.is_empty() {
        None
    } else {
        revision::resolve(
            repository.clone(),
            args.revision.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .forward::<RepositoryError>("Invalid revision")?
        .into()
    };

    let path = if args.path.length > 0 {
        Some(
            RelativePath::new_from_user_path(repository.require_path()?, args.path.as_str())
                .forward::<RepositoryError>("Invalid repository path")?,
        )
    } else {
        None
    };

    lore_revision::repository::dump::dump(repository, revision, path, args.max_depth).await
}

/// Arguments for creating a new repository at the specified URL.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(create_local)]
pub struct LoreRepositoryCreateArgs {
    /// URL to the repository
    pub repository_url: LoreString,
    /// Optional repository description
    pub description: LoreString,
    /// Optional repository ID, set to empty string to generate a new ID
    pub id: LoreString,
    /// Use the shared store instead of a local immutable store
    pub use_shared_store: u8,
    /// [Optional] Path to use for the shared store, an empty string means to use the default
    pub shared_store_path: LoreString,
}

/// Creates a new repository at the specified URL.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Repository Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryCreate`](crate::interface::LoreEvent::RepositoryCreate) | Emitted when the repository has been successfully created |
pub async fn create(
    globals: LoreGlobalArgs,
    args: LoreRepositoryCreateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, create_local).await
}

async fn create_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryCreateArgs,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&create, &args);

            let time_start = Instant::now();

            let result = create_impl(&args).await;

            log_command_done(&create, time_start);
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

async fn create_impl(args: &LoreRepositoryCreateArgs) -> Result<(), CreateError> {
    let context = execution_context();

    let repository_url = args.repository_url.as_str();
    let repository_path = context.globals().repository_path();
    let repository_path = util::path::make_absolute(repository_path)
        .forward::<CreateError>("resolving repository path")?;

    let id = RepositoryId::from_str(args.id.as_str()).unwrap_or_default();

    let global_config = GlobalConfig::load()
        .await
        .forward::<CreateError>("loading global config")?;

    let options = CreateOptions {
        id: if !id.is_zero() { Some(id) } else { None },
        description: if !args.description.is_empty() {
            Some(args.description.to_string())
        } else {
            None
        },
        shared_store_options: SharedStoreToUseConfig::from_cli_args(
            &global_config,
            args.use_shared_store,
            &args.shared_store_path,
        )
        .forward::<CreateError>("resolving shared store config")?,
    };

    lore_revision::repository::create::create(repository_url, repository_path, options).await
}

/// Optional creator and creation-time metadata to record on a new repository.
pub struct LoreRepositoryCreateMetadata {
    /// Identity to attribute repository creation to
    pub creator: LoreString,
    /// Creation timestamp (milliseconds since the Unix epoch)
    pub created: u64,
}

pub async fn create_with_metadata(
    globals: LoreGlobalArgs,
    args: LoreRepositoryCreateArgs,
    metadata: LoreRepositoryCreateMetadata,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&create, &args);

            let time_start = Instant::now();

            let result = create_with_metadata_impl(&args, &metadata).await;

            log_command_done(&create, time_start);
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

async fn create_with_metadata_impl(
    args: &LoreRepositoryCreateArgs,
    metadata: &LoreRepositoryCreateMetadata,
) -> Result<(), CreateError> {
    let context = execution_context();

    let repository_url = args.repository_url.as_str();
    let repository_path = context.globals().repository_path();

    let id = RepositoryId::from_str(args.id.as_str()).unwrap_or_default();

    let global_config = GlobalConfig::load()
        .await
        .forward::<CreateError>("loading global config")?;

    let options = CreateOptions {
        id: if !id.is_zero() { Some(id) } else { None },
        description: if !args.description.is_empty() {
            Some(args.description.to_string())
        } else {
            None
        },
        shared_store_options: SharedStoreToUseConfig::from_cli_args(
            &global_config,
            args.use_shared_store,
            &args.shared_store_path,
        )
        .forward::<CreateError>("resolving shared store config")?,
    };
    let metadata = Some(CreateMetadata {
        creator: metadata.creator.to_string(),
        created: metadata.created,
    });

    lore_revision::repository::create::create_with_metadata(
        repository_url,
        repository_path,
        options,
        metadata,
    )
    .await
}

/// Arguments for deleting a remote repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoreRepositoryDeleteArgs {
    /// URL of the remote repository to delete
    pub repository_url: LoreString,
}

pub async fn delete(
    globals: LoreGlobalArgs,
    args: LoreRepositoryDeleteArgs,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&delete, &args);

            let time_start = Instant::now();

            let repository_url = args.repository_url.as_str();

            let result = lore_revision::repository::delete::delete(
                repository_url,
                execution_context().globals().identity().unwrap_or_default(),
            )
            .await;

            log_command_done(&delete, time_start);
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for releasing cached store references for the repository path.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(release_local)]
pub struct LoreRepositoryReleaseArgs {}

/// Release all cached store references for the given repository path.
///
/// Frees in-memory store data and releases file-backed store cache entries.
/// Any active `RepositoryContext` instances for this path remain valid, but
/// once they are dropped the stores will be freed. Subsequent opens will
/// create fresh stores.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn release(
    globals: LoreGlobalArgs,
    args: LoreRepositoryReleaseArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, release_local).await
}

async fn release_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryReleaseArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, release, move |_args| {
        let path = execution_context().globals().repository_path().to_string();
        async move {
            repository::repository_release(path.as_ref() as &std::path::Path);
            Ok::<(), RepositoryError>(())
        }
    })
    .await
}

/// Arguments for waiting on outstanding asynchronous repository tasks.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(flush_local)]
pub struct LoreRepositoryFlushArgs {}

/// Waits for all outstanding asynchronous repository tasks to complete.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn flush(
    globals: LoreGlobalArgs,
    args: LoreRepositoryFlushArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, flush_local).await
}

async fn flush_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryFlushArgs,
    callback: LoreEventCallback,
) -> i32 {
    // For now we just ensure there are no outstanding tasks globally
    no_repository_call(globals, callback, args, flush, move |_args| {
        // TODO(mjansson): Make this more granular and only flush tasks for given repository
        async move {
            runtime_flush_guarded().await;
            Ok::<(), RepositoryError>(())
        }
    })
    .await
}

/// Arguments for running garbage collection on the local repository store.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(gc_local)]
pub struct LoreRepositoryGcArgs {}

/// Runs garbage collection on the local repository store to reclaim space from unreferenced data.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn gc(
    globals: LoreGlobalArgs,
    args: LoreRepositoryGcArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, gc_local).await
}

/// Runs a single full GC pass. `repository gc` always runs a full pass, so it
/// forces `globals.no_gc = 1` to suppress the automatic incremental tasks (which
/// would otherwise race the full pass), then runs the pass explicitly.
async fn gc_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryGcArgs,
    callback: LoreEventCallback,
) -> i32 {
    let mut globals = globals;
    globals.no_gc = 1;

    repository_call_write(
        globals,
        callback,
        args,
        gc,
        move |repository, _token, _args| lore_revision::repository::gc(repository),
    )
    .await
}

/// Arguments for listing all repositories available at a remote URL.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(list_local)]
pub struct LoreRepositoryListArgs {
    /// Remote URL to list repositories from
    pub url: LoreString,
}

/// Lists all repositories available at the given remote URL.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Repository Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryListEntry`](crate::interface::LoreEvent::RepositoryListEntry) | Emitted for each repository found |
pub async fn list(
    globals: LoreGlobalArgs,
    args: LoreRepositoryListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, list_local).await
}

async fn list_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryListArgs,
    callback: LoreEventCallback,
) -> i32 {
    let url = args.url.to_string();

    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&list, &args);

            let time_start = Instant::now();

            let result = repository::list::list(
                url.as_str(),
                execution_context().globals().identity().unwrap_or_default(),
            )
            .await;

            log_command_done(&list, time_start);
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for reporting the working directory status.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(status_local)]
pub struct LoreRepositoryStatusArgs {
    /// Include staged state in the report
    pub staged: u8,
    /// Reconcile against the filesystem and refresh dirty tracking.
    ///
    /// By default, status reports the currently tracked state: the
    /// staged revision (if any) plus any files and directories already
    /// marked dirty. No filesystem reads are performed beyond the existing
    /// dirty flags — clean or unmarked files on disk are not inspected even
    /// if they differ from the current revision.
    ///
    /// When enabled, the filesystem is walked under each requested path, every
    /// file is reconciled against the current revision, and dirty flags are
    /// set or cleared accordingly. The refreshed flags are persisted in the
    /// staged state so subsequent operations (commit, stage, status) see an
    /// accurate picture without rescanning.
    pub scan: u8,
    /// Verify dirty flags against the filesystem without a full scan.
    ///
    /// When enabled, files already marked dirty are re-examined individually: a
    /// dirty file whose on-disk content matches its tracked node (same size,
    /// and same content when the modification time differs) has its dirty flag
    /// cleared and is omitted from the report, unless it is also staged.
    /// Structural dirty actions (add/move/copy/delete) are always reported.
    /// The refreshed flags are persisted in the staged state.
    pub check_dirty: u8,
    /// Reset the tracked state before computing status
    pub reset: u8,
    /// Include the sync point in the report
    pub sync_point: u8,
    /// Only emit revision info, skipping all diffs
    pub revision_only: u8,
    /// Count directories and files (view-filtered) in the staged state if
    /// present, otherwise the current revision
    pub count: u8,
    /// Repository-relative paths to limit the status check to; empty checks all
    pub paths: LoreArray<LoreString>,
}

/// Reports the working directory status.
///
/// By default this lists the currently tracked state: the staged revision (if
/// any) plus all files and directories marked dirty in the repository. Dirty
/// flags are maintained by prior `lore dirty`, `lore stage`, or `lore status
/// --scan` operations, and by filesystem notifications — files modified
/// externally without going through any of those will not appear until the
/// next reconciliation.
///
/// Set [`scan`](LoreRepositoryStatusArgs::scan) to walk the filesystem under
/// each requested path, reconcile every file against the current revision,
/// and update the persisted dirty flags. Use this to recover from drift
/// between the on-disk tree and the tracked dirty state (for example after
/// external edits that bypassed notifications).
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Repository Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryStatusRevision`](crate::interface::LoreEvent::RepositoryStatusRevision) | Emitted with current and staged revision info |
/// | [`LoreEvent::RepositoryStatusFile`](crate::interface::LoreEvent::RepositoryStatusFile) | Emitted for each file with pending changes, conflict status, or untracked status |
/// | [`LoreEvent::RepositoryStatusCount`](crate::interface::LoreEvent::RepositoryStatusCount) | Emitted when `count` is set, with the directory and file count (view-filtered) of the staged state if present, otherwise the current revision |
/// | [`LoreEvent::PathIgnore`](crate::interface::LoreEvent::PathIgnore) | Emitted for each path excluded by ignore rules |
pub async fn status(
    globals: LoreGlobalArgs,
    args: LoreRepositoryStatusArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, status_local).await
}

async fn status_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryStatusArgs,
    callback: LoreEventCallback,
) -> i32 {
    // Avoid store updates during status, which is effectively read only
    // State fragments are still prioritized in local store, so prioritize
    // less file system writes of store files over accuracy in eviction/compaction
    let mut globals = globals;
    globals.no_atime = 1;

    if args.scan != 0 || args.check_dirty != 0 || args.reset != 0 {
        // Scan and check_dirty persist refreshed dirty flags in the staged
        // state and reset drops the staged anchor; all require write capability
        // (same pattern as verify_state + heal).
        repository_call_write(
            globals,
            callback,
            args,
            status,
            |repository, _token, args| {
                let options = StatusOptions {
                    staged: args.staged != 0,
                    scan: args.scan != 0,
                    check_dirty: args.check_dirty != 0,
                    reset: args.reset != 0,
                    sync_point: args.sync_point != 0,
                    revision_only: args.revision_only != 0,
                    count: args.count != 0,
                };
                status_impl(repository, args.paths, options)
            },
        )
        .await
    } else {
        repository_call_read(globals, callback, args, status, move |repository, args| {
            let options = StatusOptions {
                staged: args.staged != 0,
                scan: false,
                check_dirty: false,
                reset: false,
                sync_point: args.sync_point != 0,
                revision_only: args.revision_only != 0,
                count: args.count != 0,
            };
            status_impl(repository, args.paths, options)
        })
        .await
    }
}

async fn status_impl(
    repository: Arc<RepositoryContext>,
    paths: LoreArray<LoreString>,
    options: StatusOptions,
) -> Result<(), repository::status::StatusError> {
    let paths = if !paths.is_empty() {
        Some(
            convert_user_paths(repository.require_path()?, paths)
                .forward::<repository::status::StatusError>("converting user paths")?,
        )
    } else {
        None
    };

    lore_revision::repository::status::status(repository, paths, options).await
}

/// Arguments for verifying the integrity of the local repository state.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(verify_state_local)]
pub struct LoreRepositoryVerifyStateArgs {
    /// Repository-relative path to verify; empty verifies the whole repository
    pub path: LoreString,
    /// Heal detected inconsistencies
    pub heal: u8,
}

/// Verifies the integrity of the local repository state, optionally healing inconsistencies.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Verify Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryVerifyStateBegin`](crate::interface::LoreEvent::RepositoryVerifyStateBegin) | Emitted when verify begins |
/// | [`LoreEvent::RepositoryVerifyStateEnd`](crate::interface::LoreEvent::RepositoryVerifyStateEnd) | Emitted when verify completes (successfully or with errors) |
/// | [`LoreEvent::RepositoryVerifyFragment`](crate::interface::LoreEvent::RepositoryVerifyFragment) | Emitted for each fragment verified in the local store |
/// | [`LoreEvent::RepositoryVerifyFragmentRemote`](crate::interface::LoreEvent::RepositoryVerifyFragmentRemote) | Emitted for each fragment verified against the remote store, and when remote verification fails |
pub async fn verify_state(
    globals: LoreGlobalArgs,
    args: LoreRepositoryVerifyStateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, verify_state_local).await
}

async fn verify_state_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryVerifyStateArgs,
    callback: LoreEventCallback,
) -> i32 {
    if args.heal != 0 {
        repository_call_write(
            globals,
            callback,
            args,
            verify_state,
            |repository, _token, args| verify_state_impl(repository, args),
        )
        .await
    } else {
        repository_call_read(globals, callback, args, verify_state, verify_state_impl).await
    }
}

async fn verify_state_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRepositoryVerifyStateArgs,
) -> Result<(), RepositoryError> {
    let path = if !args.path.is_empty() {
        Some(
            RelativePath::new_from_user_path(repository.require_path()?, args.path.as_str())
                .forward::<RepositoryError>("Invalid repository path")?,
        )
    } else {
        None
    };
    lore_revision::repository::verify::verify(repository, path, args.heal != 0).await
}

/// Arguments for verifying a single fragment in the local store.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(verify_fragment_local)]
pub struct LoreRepositoryVerifyFragmentArgs {
    /// Fragment hash to verify
    pub hash: LoreString,
    /// Optional context to match; empty matches any
    pub context: LoreString,
    /// Heal detected inconsistencies during remote verification
    pub heal: u8,
}

pub async fn verify_fragment(
    globals: LoreGlobalArgs,
    args: LoreRepositoryVerifyFragmentArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, verify_fragment_local).await
}

async fn verify_fragment_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryVerifyFragmentArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        verify_fragment,
        verify_fragment_impl,
    )
    .await
}

async fn verify_fragment_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRepositoryVerifyFragmentArgs,
) -> Result<(), RepositoryError> {
    let core_args = lore_revision::repository::verify::VerifyFragmentArgs {
        hash: args.hash,
        context: args.context,
        heal: args.heal != 0,
    };
    lore_revision::repository::verify::verify_fragment(repository, core_args).await
}

/// Arguments for querying the local immutable store by fragment address.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(store_immutable_query_local)]
pub struct LoreRepositoryStoreImmutableQueryArgs {
    /// Fragment address to query
    pub address: LoreString,
    /// Recurse into and query subfragments
    pub recurse: u8,
}

/// Queries the local immutable store for fragments matching a given address.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Repository Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryStoreImmutableQuery`](crate::interface::LoreEvent::RepositoryStoreImmutableQuery) | Emitted for each fragment entry found in the immutable store |
pub async fn store_immutable_query(
    globals: LoreGlobalArgs,
    args: LoreRepositoryStoreImmutableQueryArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, store_immutable_query_local).await
}

async fn store_immutable_query_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryStoreImmutableQueryArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        store_immutable_query,
        move |repository, args| {
            lore_revision::repository::store::immutable_query(
                repository,
                args.address.to_string(),
                execution_context().globals().local(),
                args.recurse != 0,
            )
        },
    )
    .await
}

/// Arguments for retrieving repository metadata.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_get_local)]
pub struct LoreRepositoryMetadataGetArgs {
    /// Metadata key to fetch; empty string lists all entries
    pub key: LoreString,
}

/// Retrieves repository metadata. If `key` is non-empty, returns that single key's value.
/// If `key` is empty, returns all metadata entries.
pub async fn metadata_get(
    globals: LoreGlobalArgs,
    args: LoreRepositoryMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_get_local).await
}

async fn metadata_get_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        metadata_get,
        move |repository, args| {
            let key = if args.key.is_empty() {
                None
            } else {
                Some(args.key.to_string())
            };
            async move {
                lore_revision::metadata::repository::get(
                    repository,
                    key.as_deref(),
                    execution_context().globals().local(),
                )
                .await
            }
        },
    )
    .await
}

/// Arguments for setting metadata key-value pairs on the current repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_set_local)]
pub struct LoreRepositoryMetadataSetArgs {
    /// Metadata keys to set, positionally aligned with `values` and `formats`
    pub keys: LoreArray<LoreString>,
    /// Values to set, one per key, encoded per the matching `formats` entry
    pub values: LoreArray<LoreString>,
    /// Value format/type for each key-value pair
    pub formats: LoreArray<LoreMetadataType>,
}

/// Sets one or more metadata key-value pairs on the current repository.
pub async fn metadata_set(
    globals: LoreGlobalArgs,
    args: LoreRepositoryMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_set_local).await
}

async fn metadata_set_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_set,
        |repository, _token, args| metadata_set_impl(repository, args),
    )
    .await
}

async fn metadata_set_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRepositoryMetadataSetArgs,
) -> Result<(), lore_revision::metadata::repository::RepositoryMetadataError> {
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataType;

    let keys: Vec<_> = args
        .keys
        .as_slice()
        .iter()
        .map(|k| k.as_str().as_bytes())
        .collect();

    let mut encoded_values: Vec<Vec<u8>> = Vec::with_capacity(args.values.as_slice().len());
    let mut formats: Vec<MetadataType> = Vec::with_capacity(args.formats.as_slice().len());
    for (v, f) in args
        .values
        .as_slice()
        .iter()
        .zip(args.formats.as_slice().iter())
    {
        let metadata_type = (*f).into();
        encoded_values.push(
            Metadata::decode_to_value(v.as_str(), &metadata_type).map_err(|e| {
                lore_base::error::InvalidArguments {
                    reason: format!("invalid metadata value '{}': {e}", v.as_str()),
                }
            })?,
        );
        formats.push(metadata_type);
    }
    let values: Vec<&[u8]> = encoded_values.iter().map(|v| v.as_slice()).collect();

    lore_revision::metadata::repository::set(repository, &keys, &values, &formats).await
}

/// Arguments for removing metadata keys from the current repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_clear_local)]
pub struct LoreRepositoryMetadataClearArgs {
    /// Keys to clear; empty array clears all user-defined keys
    pub keys: LoreArray<LoreString>,
}

/// Removes metadata keys from the current repository.
pub async fn metadata_clear(
    globals: LoreGlobalArgs,
    args: LoreRepositoryMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_clear_local).await
}

async fn metadata_clear_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_clear,
        move |repository, _token, args| {
            let keys: Vec<String> = args.keys.as_slice().iter().map(|k| k.to_string()).collect();
            async move {
                let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
                lore_revision::metadata::repository::clear(repository, &key_refs).await
            }
        },
    )
    .await
}

// --- Instance management commands ---

/// Arguments for listing the tracked instances of the repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(instance_list_local)]
pub struct LoreRepositoryInstanceListArgs {}

pub async fn instance_list(
    globals: LoreGlobalArgs,
    args: LoreRepositoryInstanceListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, instance_list_local).await
}

async fn instance_list_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryInstanceListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        instance_list,
        move |repository, _args| lore_revision::instance::instance_list(repository),
    )
    .await
}

/// Arguments for pruning stale instances of the repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(instance_prune_local)]
pub struct LoreRepositoryInstancePruneArgs {}

pub async fn instance_prune(
    globals: LoreGlobalArgs,
    args: LoreRepositoryInstancePruneArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, instance_prune_local).await
}

async fn instance_prune_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryInstancePruneArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        instance_prune,
        move |repository, _token, _args| lore_revision::instance::instance_prune(repository),
    )
    .await
}

/// Arguments for updating the recorded path of the current repository instance.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(update_path_local)]
pub struct LoreRepositoryUpdatePathArgs {}

pub async fn repository_update_path(
    globals: LoreGlobalArgs,
    args: LoreRepositoryUpdatePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, update_path_local).await
}

async fn update_path_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryUpdatePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        repository_update_path,
        move |repository, _token, _args| lore_revision::instance::update_path(repository),
    )
    .await
}

/// Arguments for reading a value from the repository config.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(config_get_local)]
pub struct LoreRepositoryConfigGetArgs {
    /// Config key to read (`remote_url` or `identity`)
    pub key: LoreString,
}

pub async fn config_get(
    globals: LoreGlobalArgs,
    args: LoreRepositoryConfigGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, config_get_local).await
}

async fn config_get_local(
    globals: LoreGlobalArgs,
    args: LoreRepositoryConfigGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_no_store(
        globals,
        callback,
        args,
        config_get,
        move |repository, args| {
            let key = args.key.to_string();
            async move {
                let config_path = repository
                    .require_path()?
                    .join(repository.format.dot_dir())
                    .join(lore_revision::repository::CONFIG);
                let config_str = tokio::fs::read_to_string(&config_path)
                    .await
                    .internal("Failed to load config file")?;
                let config: lore_revision::repository::RepositoryConfig =
                    toml::de::from_str(&config_str).internal("Failed to load config file")?;
                let value = match key.as_str() {
                    "remote_url" => config.remote_url.unwrap_or_default(),
                    "identity" => config.identity.unwrap_or_default(),
                    _ => {
                        return Err(lore_revision::repository::RepositoryError::internal(
                            "Invalid repository path",
                        ));
                    }
                };
                lore_revision::event::LoreEvent::RepositoryConfigGet(
                    lore_revision::repository::LoreRepositoryConfigGetEventData {
                        key: LoreString::from(key.as_str()),
                        value: LoreString::from(value.as_str()),
                    },
                )
                .send();
                Ok(())
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    // Scans the handler modules for any `send_error` call on a terminal arm,
    // so a regression that re-emits a mid-stream `Error` event fails the build.
    const MIGRATED_SOURCES: &[(&str, &str)] = &[
        ("repository.rs", include_str!("repository.rs")),
        ("auth.rs", include_str!("auth.rs")),
    ];

    #[test]
    fn migrated_terminal_arms_have_no_send_error_call() {
        // Build the needle from parts so this scanning test does not match its
        // own source when it scans `repository.rs`.
        let needle = format!(".{}(", "send_error");
        for (name, source) in MIGRATED_SOURCES {
            assert!(
                !source.contains(&needle),
                "{name} still calls the dispatcher error sink on a terminal arm; \
                 the migrated handler must route the error through `complete` \
                 instead of emitting a mid-stream `Error` event"
            );
        }
    }
}
