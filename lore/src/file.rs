// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::file;
use lore_revision::file::dump::DumpError;
use lore_revision::file::hash::HashError;
use lore_revision::file::history::HistoryOptions;
use lore_revision::file::info::InfoOptions;
use lore_revision::file::obliterate::ObliterateError;
use lore_revision::file::reset::ResetOptions;
use lore_revision::file::unstage::UnstageOptions;
use lore_revision::file::write::WriteAddressOptions;
use lore_revision::file::write::WriteError;
use lore_revision::file::write::WriteFileOptions;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreMetadataType;
use lore_revision::interface::LoreString;
use lore_revision::metadata;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::set::SetError;
use lore_revision::node;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::stage;
use lore_revision::stage::StageOptions;
use lore_revision::util::path::is_path_inside_repository;
use lore_revision::util::path::make_absolute;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call_delegation::dispatch_call;
use crate::util::convert_user_paths;

/// Arguments for retrieving file information (size, hash, staged status).
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(info_local)]
pub struct LoreFileInfoArgs {
    /// Array of paths
    pub paths: LoreArray<LoreString>,
    /// Revision to get info for
    pub revision: LoreString,
    /// Calculate the filtered local filesystem hash and size
    pub local: u8,
    /// Calculate the filtered repository size
    pub filtered: u8,
}

/// Retrieves information about one or more files including size, hash, and staged status.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileInfo`](crate::interface::LoreEvent::FileInfo) | Emitted for each file with its metadata (size, hash, staged status, etc.), including local/filtered info when requested |
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreFileInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_local).await
}

async fn info_local(
    globals: LoreGlobalArgs,
    args: LoreFileInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, info, info_impl).await
}

async fn info_impl(
    repository: Arc<RepositoryContext>,
    args: LoreFileInfoArgs,
) -> Result<(), file::info::InfoError> {
    let paths = {
        convert_user_paths(repository.require_path()?, args.paths)
            .forward::<file::info::InfoError>("Invalid path")?
    };

    let options = InfoOptions {
        revision: args.revision.into(),
        local: args.local != 0,
        filtered: args.filtered != 0,
    };

    file::info::info(repository, paths, options).await
}

/// Arguments for diffing files between two revisions.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(diff_local)]
pub struct LoreFileDiffArgs {
    /// An array of paths
    pub paths: LoreArray<LoreString>,
    /// Source revision
    pub source_revision: LoreString,
    /// Target revision
    pub target_revision: LoreString,
    /// Produce three-way merge output with conflict markers
    pub diff3: u8,
    /// Number of unchanged context lines per unified-diff hunk
    pub context_lines: u32,
    /// Treat lines that differ only in trailing whitespace as equal
    pub ignore_whitespace_eol: u8,
    /// Collapse runs of internal whitespace to a single space for comparison
    pub ignore_whitespace_inline: u8,
}

/// Computes the diff of files between two revisions.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileDiff`](crate::interface::LoreEvent::FileDiff) | Emitted for each file that differs between the two revisions |
pub async fn diff(
    globals: LoreGlobalArgs,
    args: LoreFileDiffArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, diff_local).await
}

async fn diff_local(
    globals: LoreGlobalArgs,
    args: LoreFileDiffArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, diff, diff_impl).await
}

async fn diff_impl(
    repository: Arc<RepositoryContext>,
    args: LoreFileDiffArgs,
) -> Result<(), file::diff::DiffError> {
    let paths = if !args.paths.is_empty() {
        convert_user_paths(repository.require_path()?, args.paths)
            .forward::<file::diff::DiffError>("Invalid path")?
    } else {
        vec![]
    };

    file::diff::diff(
        repository,
        args.source_revision.into(),
        args.target_revision.into(),
        paths,
        args.diff3 != 0,
        file::diff::DiffOptions {
            context_lines: args.context_lines,
            ignore_whitespace_eol: args.ignore_whitespace_eol != 0,
            ignore_whitespace_inline: args.ignore_whitespace_inline != 0,
        },
    )
    .await
}

/// Arguments for clearing all metadata associated with a file.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_clear_local)]
pub struct LoreFileMetadataClearArgs {
    /// Which file to clear metadata for
    pub path: LoreString,
}

/// Clears all metadata associated with a file.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Metadata Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::MetadataClearFile`](crate::interface::LoreEvent::MetadataClearFile) | Emitted when metadata has been cleared for the file |
pub async fn metadata_clear(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_clear_local).await
}

async fn metadata_clear_local(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_clear,
        move |repository, token, args| async move {
            metadata::clear::clear_file(repository, &token, args.path.to_string()).await
        },
    )
    .await
}

/// Arguments for retrieving a single metadata value for a file by key and revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_get_local)]
pub struct LoreFileMetadataGetArgs {
    /// Revision to get metadata for
    pub revision: LoreString,
    /// Where to get metadata for
    pub path: LoreString,
    /// Metadata key
    pub key: LoreString,
}

/// Retrieves a single metadata value for a file by key and revision.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Metadata Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for the requested metadata key/value pair |
pub async fn metadata_get(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_get_local).await
}

async fn metadata_get_local(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        metadata_get,
        move |repository, args| {
            metadata::get::get_file(
                repository,
                args.revision.into(),
                args.path.to_string(),
                args.key.to_string(),
            )
        },
    )
    .await
}

/// Arguments for listing all metadata key/value pairs for a file at a revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_list_local)]
pub struct LoreFileMetadataListArgs {
    /// What to list metadata for
    pub path: LoreString,
    /// Revision to list metadata for
    pub revision: LoreString,
}

/// Lists all metadata key/value pairs associated with a file at a given revision.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Metadata Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata key/value pair associated with the file |
pub async fn metadata_list(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_list_local).await
}

async fn metadata_list_local(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        metadata_list,
        move |repository, args| {
            metadata::list::list_file(repository, args.revision.into(), args.path.to_string())
        },
    )
    .await
}

/// Arguments for setting metadata key/value pairs on one or more files.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_set_local)]
pub struct LoreFileMetadataSetArgs {
    /// An array of paths
    pub paths: LoreArray<LoreString>,
    /// An array of keys
    pub keys: LoreArray<LoreString>,
    /// An array of values
    pub values: LoreArray<LoreString>,
    /// Pointer to an array of formats
    pub formats: LoreArray<LoreMetadataType>,
    /// Pointer to an array of entry counts per path
    pub entries: LoreArray<u32>,
}

/// Sets metadata key/value pairs on one or more files.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn metadata_set(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_set_local).await
}

async fn metadata_set_local(
    globals: LoreGlobalArgs,
    args: LoreFileMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_set,
        |repository, token, args| async move { metadata_set_impl(repository, &token, args).await },
    )
    .await
}

async fn metadata_set_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreFileMetadataSetArgs,
) -> Result<(), SetError> {
    let paths = args.paths.as_slice();
    let entries = args.entries.as_slice();

    let count: u32 = entries.iter().sum();
    let keys = args.keys.as_slice();
    let values = args.values.as_slice();
    let formats = args.formats.as_slice();

    let mut path_slices = Vec::with_capacity(count as usize);
    let mut key_slices = Vec::with_capacity(count as usize);
    let mut value_slices = Vec::with_capacity(count as usize);
    let mut format_slices = Vec::with_capacity(count as usize);

    for path in paths.iter() {
        path_slices.push(path.as_str());
    }

    for key in keys.iter() {
        key_slices.push(key.as_str().as_bytes());
    }

    let mut encoded_values: Vec<Vec<u8>> = Vec::with_capacity(values.len());
    for (value, format) in values.iter().zip(formats.iter()) {
        let metadata_type = (*format).into();
        encoded_values.push(
            Metadata::decode_to_value(value.as_str(), &metadata_type).map_err(|e| {
                lore_base::error::InvalidArguments {
                    reason: format!("invalid metadata value '{}': {e}", value.as_str()),
                }
            })?,
        );
        format_slices.push(metadata_type);
    }
    for v in &encoded_values {
        value_slices.push(v.as_slice());
    }

    metadata::set::set_file(
        repository,
        token,
        &path_slices,
        &key_slices,
        &value_slices,
        &format_slices,
        entries,
    )
    .await
}

/// Arguments for staging one or more files for the next commit.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(stage_local)]
pub struct LoreFileStageArgs {
    /// An array of paths
    pub paths: LoreArray<LoreString>,
    /// Case change handling, 0 = error, 1 = update filesystem (keep), 2 = update repository (rename)
    pub case_change: u32,
    /// Force a recursive filesystem scan of directory paths (no effect on file paths)
    pub scan: u8,
}

/// Stages one or more files for inclusion in the next commit.
///
/// # Path handling
///
/// Each path in [`LoreFileStageArgs::paths`] is classified as either an
/// individual file path or a directory path:
///
/// - **Individual file paths** are always reconciled against the filesystem.
///   The file is read and its current state is staged regardless of dirty
///   flags. The [`scan`](LoreFileStageArgs::scan) flag has no effect.
/// - **Directory paths** (including the repository root) by default stage
///   only the files and child directories that are currently marked dirty in
///   the repository state — this is the fast path and relies on prior
///   notifications or `status --scan` calls to keep dirty flags accurate.
///   When [`scan`](LoreFileStageArgs::scan) is set, the directory is walked
///   recursively, every contained file is reconciled against the filesystem,
///   and the dirty flags are disregarded. Use this when you need a full
///   reconciliation (e.g. after operations that may have changed files
///   without going through the dirty-tracking path).
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Stage Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileStageBegin`](crate::interface::LoreEvent::FileStageBegin) | Emitted when staging begins, includes path count |
/// | [`LoreEvent::FileStageProgress`](crate::interface::LoreEvent::FileStageProgress) | Emitted periodically during staging with file counts |
/// | [`LoreEvent::FileStageEnd`](crate::interface::LoreEvent::FileStageEnd) | Emitted when staging completes |
/// | [`LoreEvent::FileStageRevision`](crate::interface::LoreEvent::FileStageRevision) | Emitted with the resulting staged revision, or when no changes are found |
/// | [`LoreEvent::FileStageFile`](crate::interface::LoreEvent::FileStageFile) | Emitted for each file staged or staged for deletion |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by filters |
pub async fn stage(
    globals: LoreGlobalArgs,
    args: LoreFileStageArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, stage_local).await
}

async fn stage_local(
    globals: LoreGlobalArgs,
    args: LoreFileStageArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        stage,
        move |repository, token, args| async move {
            let options = StageOptions {
                case_change: stage::StageCaseChange::from_u32(args.case_change),
                node_flags: node::NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: args.scan != 0,
            };

            file::stage::stage(repository, &token, args.paths, options).await
        },
    )
    .await
}

/// Arguments for staging one or more files as merge resolutions.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(stage_merge_local)]
pub struct LoreFileStageMergeArgs {
    /// Paths to files to stage as merge
    pub paths: LoreArray<LoreString>,
}

/// Stages one or more files as merge resolutions.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Stage Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileStageBegin`](crate::interface::LoreEvent::FileStageBegin) | Emitted when merge-staging begins |
/// | [`LoreEvent::FileStageProgress`](crate::interface::LoreEvent::FileStageProgress) | Emitted periodically during merge-staging |
/// | [`LoreEvent::FileStageRevision`](crate::interface::LoreEvent::FileStageRevision) | Emitted with the resulting staged revision |
/// | [`LoreEvent::FileStageFile`](crate::interface::LoreEvent::FileStageFile) | Emitted for each file staged |
pub async fn stage_merge(
    globals: LoreGlobalArgs,
    args: LoreFileStageMergeArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, stage_merge_local).await
}

async fn stage_merge_local(
    globals: LoreGlobalArgs,
    args: LoreFileStageMergeArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        stage_merge,
        move |repository, token, args| async move {
            let options = StageOptions {
                case_change: stage::StageCaseChange::Error,
                node_flags: node::NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            };

            file::stage::stage_merge(repository, &token, args.paths, options).await
        },
    )
    .await
}

/// Arguments for staging a file move from one path to another.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(stage_move_local)]
pub struct LoreFileStageMoveArgs {
    /// Original path of file
    pub from_path: LoreString,
    /// New path of file
    pub to_path: LoreString,
}

/// Stages a file move from one path to another.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Stage Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileStageBegin`](crate::interface::LoreEvent::FileStageBegin) | Emitted when move staging begins |
/// | [`LoreEvent::FileStageEnd`](crate::interface::LoreEvent::FileStageEnd) | Emitted when move staging completes |
/// | [`LoreEvent::FileStageRevision`](crate::interface::LoreEvent::FileStageRevision) | Emitted with the resulting staged revision |
/// | [`LoreEvent::FileStageFile`](crate::interface::LoreEvent::FileStageFile) | Emitted for deletion of the original path and for the new path being staged |
pub async fn stage_move(
    globals: LoreGlobalArgs,
    args: LoreFileStageMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, stage_move_local).await
}

async fn stage_move_local(
    globals: LoreGlobalArgs,
    args: LoreFileStageMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        stage_move,
        move |repository, token, args| async move {
            let from = args.from_path.to_string();
            let to = args.to_path.to_string();
            let options = StageOptions {
                case_change: stage::StageCaseChange::Error,
                node_flags: node::NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            };

            file::stage::stage_move(repository, &token, from, to, options).await
        },
    )
    .await
}

// ---- Dirty API ----

/// Arguments for marking files dirty in the staged state (add/modify/delete inferred from filesystem).
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(dirty_local)]
pub struct LoreFileDirtyArgs {
    /// An array of paths
    pub paths: LoreArray<LoreString>,
}

pub async fn dirty(
    globals: LoreGlobalArgs,
    args: LoreFileDirtyArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dirty_local).await
}

async fn dirty_local(
    globals: LoreGlobalArgs,
    args: LoreFileDirtyArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        dirty,
        move |repository, _token, args| file::dirty::dirty(repository, args.paths),
    )
    .await
}

/// Arguments for marking a file dirty-moved (relocates the staged node, no filesystem checks).
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(dirty_move_local)]
pub struct LoreFileDirtyMoveArgs {
    /// Original path of file
    pub from_path: LoreString,
    /// New path of file
    pub to_path: LoreString,
}

pub async fn dirty_move(
    globals: LoreGlobalArgs,
    args: LoreFileDirtyMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dirty_move_local).await
}

async fn dirty_move_local(
    globals: LoreGlobalArgs,
    args: LoreFileDirtyMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        dirty_move,
        move |repository, _token, args| {
            let from = args.from_path.to_string();
            let to = args.to_path.to_string();
            file::dirty::dirty_move(repository, from, to)
        },
    )
    .await
}

/// Arguments for marking a file dirty-copied (creates a new staged destination node, no filesystem checks).
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(dirty_copy_local)]
pub struct LoreFileDirtyCopyArgs {
    /// Source path of file
    pub from_path: LoreString,
    /// Destination path of copy
    pub to_path: LoreString,
}

pub async fn dirty_copy(
    globals: LoreGlobalArgs,
    args: LoreFileDirtyCopyArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dirty_copy_local).await
}

async fn dirty_copy_local(
    globals: LoreGlobalArgs,
    args: LoreFileDirtyCopyArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        dirty_copy,
        move |repository, _token, args| {
            let from = args.from_path.to_string();
            let to = args.to_path.to_string();
            file::dirty::dirty_copy(repository, from, to)
        },
    )
    .await
}

// ---- Unstage API ----

/// Arguments for removing one or more files from the staged changeset.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(unstage_local)]
pub struct LoreFileUnstageArgs {
    /// An array of paths
    pub paths: LoreArray<LoreString>,
}

/// Removes one or more files from the staged changeset.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Stage Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileUnstageBegin`](crate::interface::LoreEvent::FileUnstageBegin) | Emitted when unstage begins, includes path count |
/// | [`LoreEvent::FileUnstageProgress`](crate::interface::LoreEvent::FileUnstageProgress) | Emitted periodically during unstaging |
/// | [`LoreEvent::FileUnstageEnd`](crate::interface::LoreEvent::FileUnstageEnd) | Emitted when unstaging completes |
/// | [`LoreEvent::FileUnstageRevision`](crate::interface::LoreEvent::FileUnstageRevision) | Emitted with the resulting staged revision |
/// | [`LoreEvent::FileUnstageFile`](crate::interface::LoreEvent::FileUnstageFile) | Emitted for each file that was unstaged |
pub async fn unstage(
    globals: LoreGlobalArgs,
    args: LoreFileUnstageArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, unstage_local).await
}

async fn unstage_local(
    globals: LoreGlobalArgs,
    args: LoreFileUnstageArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        unstage,
        move |repository, token, args| async move {
            let options = UnstageOptions { single_node: false };
            file::unstage::unstage(repository, &token, args.paths, options).await
        },
    )
    .await
}

/// Arguments for resetting files to a revision, optionally purging untracked files.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(reset_local)]
pub struct LoreFileResetArgs {
    /// Pointer to an array of paths
    pub paths: LoreArray<LoreString>,
    /// Revision to reset files into
    pub revision: LoreString,
    /// Purge untracked files
    pub purge: u8,
}

/// Resets one or more files to a specified revision, optionally purging untracked files.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileResetBegin`](crate::interface::LoreEvent::FileResetBegin) | Emitted when reset starts, includes path count |
/// | [`LoreEvent::FileResetProgress`](crate::interface::LoreEvent::FileResetProgress) | Emitted periodically during file reset with progress counts |
/// | [`LoreEvent::FileResetEnd`](crate::interface::LoreEvent::FileResetEnd) | Emitted when reset completes |
/// | [`LoreEvent::FileResetFile`](crate::interface::LoreEvent::FileResetFile) | Emitted for each file that was reset |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file materialized |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by filters |
pub async fn reset(
    globals: LoreGlobalArgs,
    args: LoreFileResetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, reset_local).await
}

async fn reset_local(
    globals: LoreGlobalArgs,
    args: LoreFileResetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        reset,
        move |repository, token, args| async move {
            let options = ResetOptions {
                purge: args.purge != 0,
                single_node: false,
            };

            file::reset::reset(repository, &token, args.paths, args.revision, options).await
        },
    )
    .await
}

/// Arguments for resetting files to the last merged revision on a branch.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(reset_to_last_merged_local)]
pub struct LoreFileResetToLastMergedArgs {
    /// Pointer to an array of paths
    pub paths: LoreArray<LoreString>,
    /// Branch whose last merged revision to reset to
    pub branch: LoreString,
    /// Purge untracked files
    pub purge: u8,
}

/// Resets files to the state they were in at the last merged revision on a branch.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileResetBegin`](crate::interface::LoreEvent::FileResetBegin) | Emitted when reset starts |
/// | [`LoreEvent::FileResetProgress`](crate::interface::LoreEvent::FileResetProgress) | Emitted periodically during file reset |
/// | [`LoreEvent::FileResetEnd`](crate::interface::LoreEvent::FileResetEnd) | Emitted when reset completes |
/// | [`LoreEvent::FileResetFile`](crate::interface::LoreEvent::FileResetFile) | Emitted for each file that was reset |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file materialized |
pub async fn reset_to_last_merged(
    globals: LoreGlobalArgs,
    args: LoreFileResetToLastMergedArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, reset_to_last_merged_local).await
}

async fn reset_to_last_merged_local(
    globals: LoreGlobalArgs,
    args: LoreFileResetToLastMergedArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        reset,
        move |repository, _token, args| {
            let options = ResetOptions {
                purge: args.purge != 0,
                single_node: false,
            };

            file::reset::reset_to_last_merged(repository, args.paths, args.branch, options)
        },
    )
    .await
}

/// Arguments for writing a file to a destination by path/revision or by address.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(write_local)]
pub struct LoreFileWriteArgs {
    /// Address of data to write; takes precedence over `path` when non-empty
    pub address: LoreString,
    /// Repository path to the file; used when `address` is empty
    pub path: LoreString,
    /// Revision of the file to write (used with `path`)
    pub revision: LoreString,
    /// Destination filesystem path to write to
    pub output: LoreString,
}

/// Writes a file to the repository by path/revision or by address.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileWrite`](crate::interface::LoreEvent::FileWrite) | Emitted when the file has been successfully written to the repository |
/// | [`LoreEvent::FileWrite`](crate::interface::LoreEvent::FileWrite) | Emitted when writing an address-based file |
pub async fn write(
    globals: LoreGlobalArgs,
    args: LoreFileWriteArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, write_local).await
}

async fn write_local(
    globals: LoreGlobalArgs,
    args: LoreFileWriteArgs,
    callback: LoreEventCallback,
) -> i32 {
    // The destination is the only thing `write` mutates. Per the
    // `lore-revision/clippy.toml` disallow-list policy, repository-level
    // filesystem writes must hold a `RepositoryWriteToken`; if the
    // destination lies outside the repo, no repository-level write happens
    // and the command can dispatch through `repository_call_read`.
    // Resolution failures fall through to the write dispatch — the safe
    // default.
    let output_inside_repo = make_absolute(globals.repository_path.as_str())
        .map_or(true, |repo_abs| {
            is_path_inside_repository(repo_abs.as_path(), args.output.as_str())
        });

    if output_inside_repo {
        repository_call_write(
            globals,
            callback,
            args,
            write,
            |repository, token, args| async move {
                write_impl(repository, Some(&token), args).await
            },
        )
        .await
    } else {
        repository_call_read(
            globals,
            callback,
            args,
            write,
            |repository, args| async move { write_impl(repository, None, args).await },
        )
        .await
    }
}

async fn write_impl(
    repository: Arc<RepositoryContext>,
    token: Option<&RepositoryWriteToken>,
    args: LoreFileWriteArgs,
) -> Result<(), WriteError> {
    let output = args.output.to_string();

    if !args.address.is_empty() {
        let options = WriteAddressOptions {};

        let address = args.address.to_string();

        lore_revision::file::write::write_address(repository, token, address, output, options)
            .await?;
    } else {
        let options = WriteFileOptions {
            revision: args.revision.into(),
        };

        let path = args.path.to_string();

        lore_revision::file::write::write_file(repository, token, path, output, options).await?;
    }

    Ok(())
}

/// Arguments for permanently removing a file or address from repository history.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(obliterate_local)]
pub struct LoreFileObliterateArgs {
    /// Address of data to obliterate; takes precedence over `path` when non-empty
    pub address: LoreString,
    /// Repository path to obliterate; used when `address` is empty
    pub path: LoreString,
}

/// Permanently removes a file or address from repository history.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileObliterate`](crate::interface::LoreEvent::FileObliterate) | Emitted for each file permanently removed from repository history |
pub async fn obliterate(
    globals: LoreGlobalArgs,
    args: LoreFileObliterateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, obliterate_local).await
}

async fn obliterate_local(
    globals: LoreGlobalArgs,
    args: LoreFileObliterateArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        obliterate,
        |repository, token, args| async move { obliterate_impl(repository, &token, args).await },
    )
    .await
}

async fn obliterate_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreFileObliterateArgs,
) -> Result<(), ObliterateError> {
    if !args.address.is_empty() {
        let address = Address::from_str(args.address.as_str()).map_err(|_err| {
            ObliterateError::from(lore_base::error::InvalidAddress {
                address: args.address.to_string(),
            })
        })?;

        lore_revision::file::obliterate::obliterate_address(repository, address).await?;
    } else {
        let path = args.path.to_string();

        lore_revision::file::obliterate::obliterate_file(repository, token, path).await?;
    }

    Ok(())
}

/// Arguments for dumping the binary content of a file by path or address.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(dump_local)]
pub struct LoreFileDumpArgs {
    /// Address of data to dump; takes precedence over `path` when non-empty
    pub address: LoreString,
    /// Repository path to dump; used when `address` is empty
    pub path: LoreString,
}

/// Dumps the binary content of a file by path or address.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileDump`](crate::interface::LoreEvent::FileDump) | Emitted with binary content of the requested file |
pub async fn dump(
    globals: LoreGlobalArgs,
    args: LoreFileDumpArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dump_local).await
}

async fn dump_local(
    globals: LoreGlobalArgs,
    args: LoreFileDumpArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, dump, dump_impl).await
}

async fn dump_impl(
    repository: Arc<RepositoryContext>,
    args: LoreFileDumpArgs,
) -> Result<(), DumpError> {
    if args.address.length > 0 {
        let address =
            Address::from_str(args.address.as_str()).internal("invalid address for dump")?;

        lore_revision::file::dump::dump_address(repository, address).await?;
    } else {
        let path = args.path.to_string();

        lore_revision::file::dump::dump_file(repository, path).await?;
    }

    Ok(())
}

/// Arguments for computing the hash and size of one or more files.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(hash_local)]
pub struct LoreFileHashArgs {
    /// An array of paths
    pub paths: LoreArray<LoreString>,
}

/// Computes the hash and size of one or more files.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileHash`](crate::interface::LoreEvent::FileHash) | Emitted with the computed hash and size of the specified file |
pub async fn hash(
    globals: LoreGlobalArgs,
    args: LoreFileHashArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, hash_local).await
}

async fn hash_local(
    globals: LoreGlobalArgs,
    args: LoreFileHashArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, hash, hash_impl).await
}

async fn hash_impl(
    repository: Arc<RepositoryContext>,
    args: LoreFileHashArgs,
) -> Result<(), HashError> {
    for path in args.paths.as_slice().iter() {
        file::hash::hash(repository.clone(), path.as_str()).await?;
    }
    Ok(())
}

/// Arguments for retrieving the revision history of a specific file.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(history_local)]
pub struct LoreFileHistoryArgs {
    /// A path to a file
    pub path: LoreString,
    /// Optional revision specifier to start from
    pub revision: LoreString,

    /// Restrict history to revisions on this branch
    pub branch: LoreString,

    /// Number of revisions to list
    pub length: u32,

    /// Number of revisions to search initially
    pub depth: u32,
}

/// Retrieves the revision history for a specific file.
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
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted when an error occurs |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end (`status: 0` success, `status: 1` failure) |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileHistory`](crate::interface::LoreEvent::FileHistory) | Emitted for each revision in which the file was modified |
pub async fn history(
    globals: LoreGlobalArgs,
    args: LoreFileHistoryArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, history_local).await
}

async fn history_local(
    globals: LoreGlobalArgs,
    args: LoreFileHistoryArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, history, move |repository, args| {
        let path = args.path.to_string();

        let options = HistoryOptions {
            revision: args.revision.into(),
            branch: args.branch.into(),
            length: args.length,
            depth: args.depth,
        };

        file::history::history(repository, path, options)
    })
    .await
}
