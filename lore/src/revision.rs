// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(non_camel_case_types)]

use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch::merge::MergeError;
use lore_revision::commit::CommitOptions;
use lore_revision::find::FindError;
use lore_revision::find::FindOptions;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::execution_context;
use lore_revision::lore_warn;
use lore_revision::metadata;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataErrors;
use lore_revision::metadata::MetadataType;
use lore_revision::metadata::set::SetError;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::revision;
use lore_revision::revision::amend::AmendRevisionOptions;
use lore_revision::revision::bisect;
use lore_revision::revision::bisect::BisectOptions;
use lore_revision::revision::history::HistoryOptions;
use lore_revision::revision::info::InfoOptions;
use lore_revision::revision::restore::RestoreOptions;
use lore_revision::revision::sync;
use lore_revision::revision::sync::SyncOptions;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreMetadataType;
use crate::interface::LoreString;
use crate::util::convert_user_paths;

/// Arguments for committing staged changes into a new revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(commit_local)]
pub struct LoreRevisionCommitArgs {
    /// Commit message
    pub message: LoreString,
    /// If set, commit only this linked repository (mount path relative to repo root)
    pub link: LoreString,
    /// Array of link relative paths that have specific messages
    #[serde(default)]
    pub link_paths: LoreArray<LoreString>,
    /// Array of messages corresponding to each link path (parallel array with `link_paths`)
    #[serde(default)]
    pub link_messages: LoreArray<LoreString>,
    /// If set, commit only this layer (mount path relative to repo root)
    #[serde(default)]
    pub layer: LoreString,
    /// Array of layer mount paths that have specific messages
    #[serde(default)]
    pub layer_paths: LoreArray<LoreString>,
    /// Array of messages corresponding to each layer path (parallel array with `layer_paths`)
    #[serde(default)]
    pub layer_messages: LoreArray<LoreString>,
    /// Emit per-fragment write stats during the commit
    #[serde(default)]
    pub stats: u8,
}

/// Commits all staged changes to the current branch as a new revision.
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
/// ## Commit Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionCommitBegin`](crate::interface::LoreEvent::RevisionCommitBegin) | Emitted when commit begins fragmenting files |
/// | [`LoreEvent::RevisionCommitProgress`](crate::interface::LoreEvent::RevisionCommitProgress) | Emitted periodically during commit with file processing counts |
/// | [`LoreEvent::RevisionCommitEnd`](crate::interface::LoreEvent::RevisionCommitEnd) | Emitted when commit file processing completes |
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the committed revision details (hash, branch, parents) |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the committed revision |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each file fragment written or deduplicated |
pub async fn commit(
    globals: LoreGlobalArgs,
    args: LoreRevisionCommitArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, commit_local).await
}

async fn commit_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCommitArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        commit,
        move |repository, token, args| async move {
            let message = args.message.to_string();
            let link_str = args.link.to_string();
            let link = if link_str.is_empty() {
                None
            } else {
                Some(link_str)
            };
            let paths = args.link_paths.as_slice();
            let messages = args.link_messages.as_slice();
            if paths.len() != messages.len() {
                lore_revision::lore_warn!(
                    "link_paths length ({}) does not match link_messages length ({}), ignoring per-link messages",
                    paths.len(),
                    messages.len()
                );
            }
            let link_messages: std::collections::HashMap<String, String> = paths
                .iter()
                .zip(messages.iter())
                .map(|(path, msg)| (path.to_string(), msg.to_string()))
                .collect();
            let layer_str = args.layer.to_string();
            let layer = if layer_str.is_empty() {
                None
            } else {
                Some(layer_str)
            };
            let layer_paths = args.layer_paths.as_slice();
            let layer_msg_values = args.layer_messages.as_slice();
            if layer_paths.len() != layer_msg_values.len() {
                lore_revision::lore_warn!(
                    "layer_paths length ({}) does not match layer_messages length ({}), ignoring per-layer messages",
                    layer_paths.len(),
                    layer_msg_values.len()
                );
            }
            let layer_messages: std::collections::HashMap<String, String> = layer_paths
                .iter()
                .zip(layer_msg_values.iter())
                .map(|(path, msg)| (path.to_string(), msg.to_string()))
                .collect();
            let options = CommitOptions {
                message,
                link,
                link_messages,
                layer,
                layer_messages,
                stats: args.stats != 0,
            };

            // Enable upload to remote during commit unless offline or local
            let ctx = lore_revision::lore::execution_context();
            if !ctx.globals().local() && !ctx.globals().offline() {
                repository.set_disable_upload(false);
            }

            lore_revision::commit::commit(repository, &token, options).await
        },
    )
    .await
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoreRevisionCommitWithMetadataArgs {
    // Message
    pub message: LoreString,
    /// An array of keys
    pub keys: LoreArray<LoreString>,
    /// An array of values
    pub values: LoreArray<LoreString>,
    /// An array of formats
    pub formats: LoreArray<LoreMetadataType>,
}

pub async fn commit_with_metadata(
    globals: LoreGlobalArgs,
    args: LoreRevisionCommitWithMetadataArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        commit_with_metadata,
        move |repository, token, args| async move {
            let message = args.message.to_string();
            let options = CommitOptions::new(message);

            // Enable upload to remote during commit unless offline or local
            let ctx = lore_revision::lore::execution_context();
            if !ctx.globals().local() && !ctx.globals().offline() {
                repository.set_disable_upload(false);
            }

            lore_revision::commit::commit_with_metadata(
                repository,
                &token,
                options,
                args.keys,
                args.values,
                args.formats,
            )
            .await
        },
    )
    .await
}

/// Arguments for amending the most recent revision's commit message.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(amend_local)]
pub struct LoreRevisionAmendArgs {
    /// New commit message
    pub message: LoreString,
}

/// Amends the most recent revision by updating its commit message.
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
/// ## Commit Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the amended revision details |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the amended revision |
pub async fn amend(
    globals: LoreGlobalArgs,
    args: LoreRevisionAmendArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, amend_local).await
}

async fn amend_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionAmendArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        amend,
        move |repository, token, args| async move {
            let message = args.message.to_string();
            let options = AmendRevisionOptions {
                message: Some(message),
            };
            lore_revision::revision::amend::amend_revision(repository, &token, options).await
        },
    )
    .await
}

/// Arguments for retrieving metadata and file information for a revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(info_local)]
pub struct LoreRevisionInfoArgs {
    /// Revision to get info for; empty for current
    pub revision: LoreString,
    /// Include delta against parent
    pub delta: u8,
    /// Include file metadata entries
    pub metadata: u8,
}

/// Retrieves metadata and file information for the specified revision.
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
/// ## Revision Info Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionInfo`](crate::interface::LoreEvent::RevisionInfo) | Emitted with revision metadata (hash, branch, parents, file count, etc.) |
/// | [`LoreEvent::RevisionInfoDelta`](crate::interface::LoreEvent::RevisionInfoDelta) | Emitted with delta information between the revision and its parent (when `delta=true`) |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata key/value of the revision (when `metadata=true`) |
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreRevisionInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_local).await
}

async fn info_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, info, move |repository, args| {
        let options = InfoOptions {
            signature: args.revision.into(),
            delta: args.delta != 0,
            metadata: args.metadata != 0,
        };
        lore_revision::revision::info::info(repository, options)
    })
    .await
}

/// Arguments for clearing all metadata from the current revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_clear_local)]
pub struct LoreRevisionMetadataClearArgs {}

/// Clears all metadata from the current revision.
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
/// ## Metadata Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::MetadataClearRevision`](crate::interface::LoreEvent::MetadataClearRevision) | Emitted when metadata has been cleared for the current revision |
pub async fn metadata_clear(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_clear_local).await
}

async fn metadata_clear_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_clear,
        move |repository, token, _args| async move {
            metadata::clear::clear_revision(repository, &token).await
        },
    )
    .await
}

/// Arguments for retrieving a single metadata value by key from a revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_get_local)]
pub struct LoreRevisionMetadataGetArgs {
    /// Metadata key to look up
    pub key: LoreString,
    /// Revision to get metadata for; empty for current
    pub revision: LoreString,
}

/// Retrieves a single metadata value by key from the specified revision.
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
/// ## Metadata Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted with the requested key/value for the revision |
pub async fn metadata_get(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_get_local).await
}

async fn metadata_get_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, metadata_get, metadata_get_impl).await
}

async fn metadata_get_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRevisionMetadataGetArgs,
) -> Result<(), MetadataErrors> {
    metadata::get::get_revision(repository, args.revision.into(), args.key.as_str()).await
}

/// Arguments for listing all metadata key/value pairs of a revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_list_local)]
pub struct LoreRevisionMetadataListArgs {
    /// Revision to list metadata for; empty for current
    pub revision: LoreString,
}

/// Lists all metadata key/value pairs associated with the specified revision.
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
/// ## Metadata Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata key/value associated with the revision |
pub async fn metadata_list(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_list_local).await
}

async fn metadata_list_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        metadata_list,
        move |repository, args| {
            let revision = if !args.revision.is_empty() {
                Some(args.revision.to_string())
            } else {
                None
            };
            metadata::list::list_revision(repository, revision)
        },
    )
    .await
}

/// Arguments for setting metadata key/value pairs on the current revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_set_local)]
pub struct LoreRevisionMetadataSetArgs {
    /// Metadata keys (parallel with `values` and `formats`)
    pub keys: LoreArray<LoreString>,
    /// Metadata values, decoded per the matching format
    pub values: LoreArray<LoreString>,
    /// Value type for each entry
    pub formats: LoreArray<LoreMetadataType>,
}

/// Sets one or more metadata key/value pairs on the current revision.
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
pub async fn metadata_set(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_set_local).await
}

async fn metadata_set_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionMetadataSetArgs,
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
    args: LoreRevisionMetadataSetArgs,
) -> Result<(), SetError> {
    let keys: Vec<_> = args
        .keys
        .as_slice()
        .iter()
        .map(|k| k.as_str().as_bytes())
        .collect();

    let mut encoded_values: Vec<Vec<u8>> = Vec::with_capacity(args.values.as_slice().len());
    let mut formats: Vec<MetadataType> = Vec::with_capacity(args.formats.as_slice().len());
    for (value, format) in args
        .values
        .as_slice()
        .iter()
        .zip(args.formats.as_slice().iter())
    {
        let metadata_type = (*format).into();
        encoded_values.push(
            Metadata::decode_to_value(value.as_str(), &metadata_type).map_err(|e| {
                lore_base::error::InvalidArguments {
                    reason: format!("invalid metadata value '{}': {e}", value.as_str()),
                }
            })?,
        );
        formats.push(metadata_type);
    }
    let values: Vec<&[u8]> = encoded_values.iter().map(|v| v.as_slice()).collect();

    metadata::set::set_revision(repository, token, &keys, &values, &formats).await
}

/// Arguments for retrieving the revision history of a branch or revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(history_local)]
pub struct LoreRevisionHistoryArgs {
    /// Start from this revision; empty for current
    pub revision: LoreString,
    /// Restrict to this branch; empty for current
    pub branch: LoreString,
    /// Stop at revisions created before this date (Unix timestamp; 0 disables)
    pub date: u64,
    /// Maximum number of revisions to return; 0 for unlimited
    pub length: u32,
    /// Stop when reaching a different branch
    pub only_branch: u8,
}

/// Retrieves the revision history for the current branch or a specified revision.
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
/// ## History Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionHistory`](crate::interface::LoreEvent::RevisionHistory) | Emitted once with summary info before entries stream |
/// | [`LoreEvent::RevisionHistoryEntry`](crate::interface::LoreEvent::RevisionHistoryEntry) | Emitted for each revision in the history |
pub async fn history(
    globals: LoreGlobalArgs,
    args: LoreRevisionHistoryArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, history_local).await
}

async fn history_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionHistoryArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, history, move |repository, args| {
        let options = HistoryOptions {
            revision: args.revision.into(),
            branch: args.branch.into(),
            date: args.date,
            length: args.length,
            only_branch: args.only_branch != 0,
        };
        lore_revision::revision::history::history(repository, options)
    })
    .await
}

/// Arguments for restoring the current branch to a previously synced revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(restore_local)]
pub struct LoreRevisionRestoreArgs {
    /// Commit message for the restored revision
    pub message: LoreString,
}

/// Restores the current branch to a previously synced revision, downloading fragments as needed.
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
/// ## Restore Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionRestoreFileBegin`](crate::interface::LoreEvent::RevisionRestoreFileBegin) | Emitted when restore starts processing files |
/// | [`LoreEvent::RevisionRestoreFile`](crate::interface::LoreEvent::RevisionRestoreFile) | Emitted for each file being restored |
/// | [`LoreEvent::RevisionRestoreFileEnd`](crate::interface::LoreEvent::RevisionRestoreFileEnd) | Emitted when file processing completes |
/// | [`LoreEvent::RevisionRestoreFragmentBegin`](crate::interface::LoreEvent::RevisionRestoreFragmentBegin) | Emitted when fragment download begins for a file |
/// | [`LoreEvent::RevisionRestoreFragmentProgress`](crate::interface::LoreEvent::RevisionRestoreFragmentProgress) | Emitted periodically during fragment download |
/// | [`LoreEvent::RevisionRestoreFragmentEnd`](crate::interface::LoreEvent::RevisionRestoreFragmentEnd) | Emitted when fragment download completes |
/// | [`LoreEvent::RevisionRestoreRevision`](crate::interface::LoreEvent::RevisionRestoreRevision) | Emitted with the restored revision details |
///
/// ## Commit Events (auto-commit of the restored revision)
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionCommitBegin`](crate::interface::LoreEvent::RevisionCommitBegin) | Emitted when auto-commit of the restored revision starts |
/// | [`LoreEvent::RevisionCommitProgress`](crate::interface::LoreEvent::RevisionCommitProgress) | Emitted periodically during auto-commit |
/// | [`LoreEvent::RevisionCommitEnd`](crate::interface::LoreEvent::RevisionCommitEnd) | Emitted when auto-commit file processing completes |
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the committed restored revision |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the restored revision |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each fragment written during restore commit |
pub async fn restore(
    globals: LoreGlobalArgs,
    args: LoreRevisionRestoreArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, restore_local).await
}

async fn restore_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRestoreArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        restore,
        move |repository, token, args| async move {
            let options = RestoreOptions {
                message: args.message.into(),
            };
            lore_revision::revision::restore::restore(repository, &token, options).await
        },
    )
    .await
}

/// Arguments for synchronizing the working directory to a target revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(sync_local)]
pub struct LoreRevisionSyncArgs {
    /// Revision to synchronize to; empty for branch tip
    pub revision: LoreString,
    /// Fast forward and keep local changes when syncing to a local revision
    pub forward_changes: u8,
    /// Reset local modified files to match the incoming revision
    pub reset: u8,
    /// Root files for dependency-based selective sync
    pub root_files: LoreArray<LoreString>,
    /// Tags to filter dependencies by during resolution
    pub dependency_tags: LoreArray<LoreString>,
    /// Follow transitive dependencies recursively
    pub dependency_recursive: u8,
    /// Maximum dependency traversal depth; 0 means unlimited
    pub dependency_depth_limit: u32,
}

/// Synchronizes the working directory to a target revision, optionally merging divergent branches.
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
/// ## Sync Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionSyncTarget`](crate::interface::LoreEvent::RevisionSyncTarget) | Emitted once after resolving the target revision with source/target info, branch, and remote URL |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file deleted, modified, added, or merged during sync |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted periodically during file realization and once at completion with cumulative counts |
/// | [`LoreEvent::RevisionSyncRevision`](crate::interface::LoreEvent::RevisionSyncRevision) | Emitted with the resulting revision when a merge occurs and at the end of sync |
/// | [`LoreEvent::RevisionResolve`](crate::interface::LoreEvent::RevisionResolve) | Emitted when resolving a partial or numbered revision reference |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by view or ignore filters |
/// | [`LoreEvent::FileStageFile`](crate::interface::LoreEvent::FileStageFile) | Emitted for each file staged for deletion during merge realization |
///
/// ## Merge Events (when local and remote branches have diverged)
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeStartBegin`](crate::interface::LoreEvent::BranchMergeStartBegin) | Emitted when an auto-merge is initiated due to divergent branches |
/// | [`LoreEvent::BranchMergeStartEnd`](crate::interface::LoreEvent::BranchMergeStartEnd) | Emitted when the auto-merge operation completes, includes sync stats and conflict flag |
/// | [`LoreEvent::BranchMergeConflictFile`](crate::interface::LoreEvent::BranchMergeConflictFile) | Emitted for each file with an unresolved merge conflict |
///
/// ## Commit Events (when an auto-merge commits with no conflicts)
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionCommitBegin`](crate::interface::LoreEvent::RevisionCommitBegin) | Emitted when auto-merge auto-commit starts fragmenting files |
/// | [`LoreEvent::RevisionCommitProgress`](crate::interface::LoreEvent::RevisionCommitProgress) | Emitted periodically during auto-commit with file processing counts |
/// | [`LoreEvent::RevisionCommitEnd`](crate::interface::LoreEvent::RevisionCommitEnd) | Emitted when auto-commit file processing completes |
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the committed merge revision details |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the auto-merge commit |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each file fragment written or deduplicated during auto-merge commit |
pub async fn sync(
    globals: LoreGlobalArgs,
    args: LoreRevisionSyncArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, sync_local).await
}

async fn sync_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionSyncArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        sync,
        move |repository, token, args| async move {
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
            let options = SyncOptions {
                revision: args.revision.into(),
                forward_changes: args.forward_changes != 0,
                reset: args.reset != 0,
                force_hash_check: false,
                filter_mode: lore_revision::filter::FilterMode::View,
                root_files,
                dependency_tags,
                dependency_recursive: args.dependency_recursive != 0,
                dependency_depth_limit: args.dependency_depth_limit,
            };

            sync::sync(repository, &token, options).await
        },
    )
    .await
}

/// Arguments for bisecting the revision range between two revisions.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoreRevisionBisectArgs {
    /// Starting (known-good) revision of the bisect range
    pub start: LoreString,
    /// Ending (known-bad) revision of the bisect range
    pub end: LoreString,
}

pub async fn bisect(
    globals: LoreGlobalArgs,
    args: LoreRevisionBisectArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        bisect,
        move |repository, token, args| async move {
            let options = BisectOptions {
                start: args.start.to_string(),
                end: args.end.to_string(),
            };
            bisect::bisect(repository, &token, options).await
        },
    )
    .await
}

/// Arguments for finding revisions by metadata or revision number.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(find_local)]
pub struct LoreRevisionFindArgs {
    /// Metadata key to search for; non-empty selects key/value search
    pub key: LoreString,
    /// Metadata value to match against `key`
    pub value: LoreString,
    /// Revision number to search for when `key` is empty; 0 disables
    pub number: u64,
}

/// Finds revisions matching a metadata key/value pair or revision number.
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
/// ## Find Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionFind`](crate::interface::LoreEvent::RevisionFind) | Emitted for each matching revision found (exact or partial match) |
pub async fn find(
    globals: LoreGlobalArgs,
    args: LoreRevisionFindArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, find_local).await
}

pub async fn find_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionFindArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, find, find_impl).await
}

async fn find_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRevisionFindArgs,
) -> Result<(), FindError> {
    let options = if args.key.length > 0 {
        FindOptions::KeyValue {
            key: args.key,
            value: args.value,
        }
    } else if args.number > 0 {
        FindOptions::Number(args.number)
    } else {
        lore_warn!("Nothing to find - specify metadata or revision number");
        return Err(FindError::internal("no revision specified"));
    };

    lore_revision::find::find_impl(repository, options).await
}

/// Arguments for computing file-level differences between two revisions.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(diff_local)]
pub struct LoreRevisionDiffArgs {
    /// Source revision to diff from
    pub revision_source: LoreString,
    /// Target revision to diff to; empty for current
    pub revision_target: LoreString,
    /// Repository-relative paths to restrict the diff to; empty for all
    pub paths: LoreArray<LoreString>,
}

/// Computes the file-level differences between two revisions.
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
/// ## Diff Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionDiffFile`](crate::interface::LoreEvent::RevisionDiffFile) | Emitted for each file that differs between the two revisions |
/// | [`LoreEvent::RevisionResolve`](crate::interface::LoreEvent::RevisionResolve) | Emitted when resolving a partial or numbered revision reference |
pub async fn diff(
    globals: LoreGlobalArgs,
    args: LoreRevisionDiffArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, diff_local).await
}

async fn diff_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionDiffArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, diff, diff_impl).await
}

async fn diff_impl(
    repository: Arc<RepositoryContext>,
    args: LoreRevisionDiffArgs,
) -> Result<(), revision::diff::DiffError> {
    let paths = if !args.paths.is_empty() {
        Some(
            convert_user_paths(repository.require_path()?, args.paths)
                .forward::<revision::diff::DiffError>("invalid path argument")?,
        )
    } else {
        None
    };

    let source_hash = revision::resolve(
        repository.clone(),
        args.revision_source.as_str(),
        execution_context().globals().search_limit(),
        execution_context().globals().search_location(),
    )
    .await
    .forward::<revision::diff::DiffError>("invalid revision")?;

    // No target provided, use current revision
    let target_hash = if args.revision_target.is_empty() {
        lore_revision::instance::load_current_anchor(&repository)
            .await
            .map(|(revision, _branch)| revision)
            .unwrap_or_default()
    } else {
        revision::resolve(
            repository.clone(),
            args.revision_target.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .forward::<revision::diff::DiffError>("invalid revision")?
    };

    lore_revision::revision::diff::diff(repository, source_hash, target_hash, paths).await
}

/// Arguments for cherry-picking a revision onto the current branch.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_local)]
pub struct LoreRevisionCherryPickArgs {
    /// Revision to cherry pick
    pub revision: LoreString,
    /// Message to use for an auto-commit if no conflicts arise
    pub message: LoreString,
    /// Disable auto-commit even if no conflicts arise
    pub no_commit: u8,
}

pub async fn cherry_pick(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_local).await
}

pub async fn cherry_pick_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick,
        async move |repository, token, args| {
            let target_revision = revision::resolve(
                repository.clone(),
                args.revision.as_str(),
                execution_context().globals().search_limit(),
                execution_context().globals().search_location(),
            )
            .await
            .forward::<MergeError>("resolving cherry-pick revision")?;

            let options = revision::cherry_pick::CherryPickOptions {
                message: args.message.to_string(),
                no_commit: args.no_commit != 0,
            };

            revision::cherry_pick::cherry_pick(repository, &token, target_revision, options).await
        },
    )
    .await
}

/// Arguments for aborting a cherry-pick operation in progress.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_abort_local)]
pub struct LoreRevisionCherryPickAbortArgs {}

pub async fn cherry_pick_abort(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickAbortArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_abort_local).await
}

async fn cherry_pick_abort_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickAbortArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick_abort,
        move |repository, _token, _args| revision::cherry_pick::cherry_pick_abort(repository),
    )
    .await
}

/// Arguments for marking cherry-pick paths as unresolved again.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_unresolve_local)]
pub struct LoreRevisionCherryPickUnresolveArgs {
    /// Repository-relative paths to mark unresolved
    pub paths: LoreArray<LoreString>,
}

pub async fn cherry_pick_unresolve(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickUnresolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_unresolve_local).await
}

async fn cherry_pick_unresolve_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickUnresolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick_unresolve,
        move |repository, token, args| async move {
            revision::cherry_pick::cherry_pick_unresolve(repository, &token, args.paths).await
        },
    )
    .await
}

/// Arguments for restarting cherry-pick conflict resolution for paths.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_restart_local)]
pub struct LoreRevisionCherryPickRestartArgs {
    /// Repository-relative paths to re-materialize for resolution
    pub paths: LoreArray<LoreString>,
}

pub async fn cherry_pick_restart(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickRestartArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_restart_local).await
}

async fn cherry_pick_restart_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickRestartArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick_restart,
        move |repository, token, args| async move {
            revision::cherry_pick::cherry_pick_restart(repository, &token, args.paths).await
        },
    )
    .await
}

/// Arguments for marking cherry-pick conflicts as resolved for paths.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_resolve_local)]
pub struct LoreRevisionCherryPickResolveArgs {
    /// Repository-relative paths to mark resolved
    pub paths: LoreArray<LoreString>,
}

pub async fn cherry_pick_resolve(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_resolve_local).await
}

async fn cherry_pick_resolve_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick_resolve,
        move |repository, token, args| async move {
            revision::cherry_pick::cherry_pick_resolve(repository, &token, args.paths).await
        },
    )
    .await
}

/// Arguments for resolving cherry-pick conflicts by keeping the "mine" version.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_resolve_mine_local)]
pub struct LoreRevisionCherryPickResolveMineArgs {
    /// Repository-relative paths to resolve in favor of "mine"
    pub paths: LoreArray<LoreString>,
}

pub async fn cherry_pick_resolve_mine(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickResolveMineArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_resolve_mine_local).await
}

async fn cherry_pick_resolve_mine_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickResolveMineArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick_resolve_mine,
        move |repository, token, args| async move {
            revision::cherry_pick::cherry_pick_resolve_mine(repository, &token, args.paths)
                .await
                .forward::<MergeError>("resolving cherry-pick with mine")
        },
    )
    .await
}

/// Arguments for resolving cherry-pick conflicts by keeping the "theirs" version.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(cherry_pick_resolve_theirs_local)]
pub struct LoreRevisionCherryPickResolveTheirsArgs {
    /// Repository-relative paths to resolve in favor of "theirs"
    pub paths: LoreArray<LoreString>,
}

pub async fn cherry_pick_resolve_theirs(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickResolveTheirsArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, cherry_pick_resolve_theirs_local).await
}

async fn cherry_pick_resolve_theirs_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionCherryPickResolveTheirsArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        cherry_pick_resolve_theirs,
        move |repository, token, args| async move {
            revision::cherry_pick::cherry_pick_resolve_theirs(repository, &token, args.paths)
                .await
                .forward::<MergeError>("resolving cherry-pick with theirs")
        },
    )
    .await
}

/// Arguments for reverting the working directory to a specified revision.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_local)]
pub struct LoreRevisionRevertArgs {
    /// Revision to revert
    pub revision: LoreString,
    /// Message to use for an auto-commit if no conflicts arise
    pub message: LoreString,
    /// Disable auto-commit even if no conflicts arise
    pub no_commit: u8,
}

/// Reverts the working directory to the specified revision, applying the inverse of its changes.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertStartBegin`](crate::interface::LoreEvent::RevertStartBegin) | Emitted when revert begins, includes target revision info |
/// | [`LoreEvent::RevertStartEnd`](crate::interface::LoreEvent::RevertStartEnd) | Emitted when revert completes, includes conflict flag |
/// | [`LoreEvent::RevertConflictFile`](crate::interface::LoreEvent::RevertConflictFile) | Emitted for each file with an unresolved revert conflict |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during the `apply_diff` phase |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file modified during revert realization |
/// | [`LoreEvent::FileStageFile`](crate::interface::LoreEvent::FileStageFile) | Emitted for each file staged for deletion during revert |
///
/// ## Commit Events (when `no_commit` is false and no conflicts arise)
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevisionCommitBegin`](crate::interface::LoreEvent::RevisionCommitBegin) | Emitted when auto-commit starts |
/// | [`LoreEvent::RevisionCommitProgress`](crate::interface::LoreEvent::RevisionCommitProgress) | Emitted during auto-commit |
/// | [`LoreEvent::RevisionCommitEnd`](crate::interface::LoreEvent::RevisionCommitEnd) | Emitted when auto-commit completes |
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the committed revert revision |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the auto-commit |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each fragment written during auto-commit |
pub async fn revert(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_local).await
}

pub async fn revert_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert,
        async move |repository, token, args| {
            let target_revision = revision::resolve(
                repository.clone(),
                args.revision.as_str(),
                execution_context().globals().search_limit(),
                execution_context().globals().search_location(),
            )
            .await
            .forward::<MergeError>("resolving revert revision")?;

            let options = revision::revert::RevertOptions {
                message: args.message.to_string(),
                no_commit: args.no_commit != 0,
            };

            revision::revert::revert(repository, &token, target_revision, options).await
        },
    )
    .await
}

/// Arguments for aborting a revert operation in progress.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_abort_local)]
pub struct LoreRevisionRevertAbortArgs {}

/// Aborts a revert operation in progress and restores the working directory to its prior state.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertAbortBegin`](crate::interface::LoreEvent::RevertAbortBegin) | Emitted when revert abort begins |
/// | [`LoreEvent::RevertAbortEnd`](crate::interface::LoreEvent::RevertAbortEnd) | Emitted when revert abort completes |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization while reverting |
pub async fn revert_abort(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertAbortArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_abort_local).await
}

async fn revert_abort_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertAbortArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert_abort,
        move |repository, _token, _args| revision::revert::revert_abort(repository),
    )
    .await
}

/// Arguments for marking revert paths as unresolved again.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_unresolve_local)]
pub struct LoreRevisionRevertUnresolveArgs {
    /// Repository-relative paths to mark unresolved
    pub paths: LoreArray<LoreString>,
}

/// Marks the specified paths as unresolved again during a revert operation.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertUnresolveFile`](crate::interface::LoreEvent::RevertUnresolveFile) | Emitted for each file marked as unresolved |
/// | [`LoreEvent::RevertUnresolveRevision`](crate::interface::LoreEvent::RevertUnresolveRevision) | Emitted with the updated staged revision after unresolve completes |
pub async fn revert_unresolve(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertUnresolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_unresolve_local).await
}

async fn revert_unresolve_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertUnresolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert_unresolve,
        move |repository, token, args| async move {
            revision::revert::revert_unresolve(repository, &token, args.paths).await
        },
    )
    .await
}

/// Arguments for restarting revert conflict resolution for paths.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_restart_local)]
pub struct LoreRevisionRevertRestartArgs {
    /// Repository-relative paths to re-materialize for resolution
    pub paths: LoreArray<LoreString>,
}

/// Restarts a revert operation in progress, re-materializing conflicted files.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertConflictFile`](crate::interface::LoreEvent::RevertConflictFile) | Emitted for each file with a remaining revert conflict |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file re-materialized during restart |
pub async fn revert_restart(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertRestartArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_restart_local).await
}

async fn revert_restart_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertRestartArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert_restart,
        move |repository, token, args| async move {
            revision::revert::revert_restart(repository, &token, args.paths).await
        },
    )
    .await
}

/// Arguments for marking revert conflicts as resolved for paths.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_resolve_local)]
pub struct LoreRevisionRevertResolveArgs {
    /// Repository-relative paths to mark resolved
    pub paths: LoreArray<LoreString>,
}

/// Marks the specified conflicting paths as resolved after a revert operation.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertResolveFile`](crate::interface::LoreEvent::RevertResolveFile) | Emitted for each file that was marked as resolved |
/// | [`LoreEvent::RevertResolveRevision`](crate::interface::LoreEvent::RevertResolveRevision) | Emitted with the updated staged revision after resolve completes |
pub async fn revert_resolve(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_resolve_local).await
}

async fn revert_resolve_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert_resolve,
        move |repository, token, args| async move {
            revision::revert::revert_resolve(repository, &token, args.paths).await
        },
    )
    .await
}

/// Arguments for resolving revert conflicts by keeping the "mine" version.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_resolve_mine_local)]
pub struct LoreRevisionRevertResolveMineArgs {
    /// Repository-relative paths to resolve in favor of "mine"
    pub paths: LoreArray<LoreString>,
}

/// Resolves revert conflicts for the specified paths by accepting the "mine" version.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertResolveFile`](crate::interface::LoreEvent::RevertResolveFile) | Emitted for each file resolved by keeping "mine" |
/// | [`LoreEvent::RevertResolveRevision`](crate::interface::LoreEvent::RevertResolveRevision) | Emitted with the updated staged revision after resolve completes |
pub async fn revert_resolve_mine(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertResolveMineArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_resolve_mine_local).await
}

async fn revert_resolve_mine_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertResolveMineArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert_resolve_mine,
        move |repository, token, args| async move {
            revision::revert::revert_resolve_mine(repository, &token, args.paths)
                .await
                .forward::<MergeError>("resolving revert with mine")
        },
    )
    .await
}

/// Arguments for resolving revert conflicts by keeping the "theirs" version.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(revert_resolve_theirs_local)]
pub struct LoreRevisionRevertResolveTheirsArgs {
    /// Repository-relative paths to resolve in favor of "theirs"
    pub paths: LoreArray<LoreString>,
}

/// Resolves revert conflicts for the specified paths by accepting the "theirs" version.
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
/// ## Revert Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RevertResolveFile`](crate::interface::LoreEvent::RevertResolveFile) | Emitted for each file resolved by keeping "theirs" |
/// | [`LoreEvent::RevertResolveRevision`](crate::interface::LoreEvent::RevertResolveRevision) | Emitted with the updated staged revision after resolve completes |
pub async fn revert_resolve_theirs(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertResolveTheirsArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revert_resolve_theirs_local).await
}

async fn revert_resolve_theirs_local(
    globals: LoreGlobalArgs,
    args: LoreRevisionRevertResolveTheirsArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        revert_resolve_theirs,
        move |repository, token, args| async move {
            revision::revert::revert_resolve_theirs(repository, &token, args.paths)
                .await
                .forward::<MergeError>("resolving revert with theirs")
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_args_old_payload_missing_layer_fields_uses_defaults() {
        // Old IPC client payload with no layer_* fields. The new fields must be
        // `#[serde(default)]` so old clients keep working.
        let payload = r#"{
            "message": "main",
            "link": "",
            "link_paths": [],
            "link_messages": []
        }"#;

        let args: LoreRevisionCommitArgs =
            serde_json::from_str(payload).expect("old payload must deserialise");

        assert_eq!(args.message.as_str(), "main");
        assert_eq!(args.link.as_str(), "");
        assert_eq!(args.layer.as_str(), "");
        assert!(args.layer_paths.as_slice().is_empty());
        assert!(args.layer_messages.as_slice().is_empty());
    }

    #[test]
    fn commit_args_new_payload_carries_layer_fields() {
        let payload = r#"{
            "message": "main",
            "link": "",
            "link_paths": [],
            "link_messages": [],
            "layer": "external/lib",
            "layer_paths": ["external/lib"],
            "layer_messages": ["layer-specific message"]
        }"#;

        let args: LoreRevisionCommitArgs =
            serde_json::from_str(payload).expect("new payload must deserialise");

        assert_eq!(args.layer.as_str(), "external/lib");
        assert_eq!(args.layer_paths.as_slice().len(), 1);
        assert_eq!(args.layer_paths.as_slice()[0].as_str(), "external/lib");
        assert_eq!(args.layer_messages.as_slice().len(), 1);
        assert_eq!(
            args.layer_messages.as_slice()[0].as_str(),
            "layer-specific message"
        );
    }
}
