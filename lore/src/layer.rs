// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::layer::LayerError;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreString;

mod add;
mod list;
mod remove;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(layer_add_local)]
/// Arguments for adding a layer from a source repository into the current repository.
pub struct LoreLayerAddArgs {
    /// Path in the current repository where the layer should be placed
    pub target_path: LoreString,
    /// Repository to add as a layer
    pub source_repository: LoreString,
    /// Path in the layer repository where the layer should start
    pub source_path: LoreString,
    /// Metadata key to use to match revisions
    pub metadata: LoreString,
}

/// Adds a layer from a source repository into the current repository at the specified path.
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
/// ## Layer Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LayerAdd`](crate::interface::LoreEvent::LayerAdd) | Emitted when a layer has been successfully added |
pub async fn layer_add(
    globals: LoreGlobalArgs,
    args: LoreLayerAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, layer_add_local).await
}

async fn layer_add_local(
    globals: LoreGlobalArgs,
    args: LoreLayerAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        layer_add,
        |repository, token, args| async move { layer_add_impl(repository, &token, args).await },
    )
    .await
}

async fn layer_add_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreLayerAddArgs,
) -> Result<(), LayerError> {
    let target_path =
        RelativePath::new_from_user_path(repository.require_path()?, args.target_path.as_str())
            .forward_with::<LayerError, _>(|| format!("Invalid path {}", args.target_path))?;
    let source_path = RelativePath::new_from_initial_path(args.source_path.as_str())
        .forward_with::<LayerError, _>(|| format!("Invalid path {}", args.source_path))?;

    add::add(
        repository,
        token,
        target_path,
        args.source_repository.as_str(),
        source_path,
        args.metadata.into(),
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(layer_remove_local)]
/// Arguments for removing a layer from the repository at the specified path.
pub struct LoreLayerRemoveArgs {
    /// Path in the current repository where the layer is placed
    pub target_path: LoreString,
    /// Repository added as a layer at the given path
    pub source_repository: LoreString,
    /// Remove all untracked files and directories inside the layer mount
    pub purge: u8,
}

/// Removes a layer from the repository at the specified path.
///
/// Tracked files are unlinked and empty directories collapsed. Untracked files
/// remain and keep their parent directories alive. The call fails with
/// `LocalModifications` when any tracked file has been locally modified,
/// unless the global `--force` flag is set, in which case modified files are
/// discarded. The optional `purge` flag turns the cleanup into a full recursive
/// delete of the layer's mount, removing untracked files and all directories.
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
/// ## Layer Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LayerRemove`](crate::interface::LoreEvent::LayerRemove) | Emitted when a layer has been successfully removed |
pub async fn layer_remove(
    globals: LoreGlobalArgs,
    args: LoreLayerRemoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, layer_remove_local).await
}

async fn layer_remove_local(
    globals: LoreGlobalArgs,
    args: LoreLayerRemoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        layer_remove,
        |repository, token, args| async move { layer_remove_impl(repository, &token, args).await },
    )
    .await
}

async fn layer_remove_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreLayerRemoveArgs,
) -> Result<(), LayerError> {
    let target_path =
        RelativePath::new_from_user_path(repository.require_path()?, args.target_path.as_str())
            .forward_with::<LayerError, _>(|| format!("Invalid path {}", args.target_path))?;

    remove::remove(
        repository,
        token,
        target_path,
        args.source_repository.as_str(),
        args.purge != 0,
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(layer_list_local)]
/// Arguments for listing all layers configured in the repository (no parameters).
pub struct LoreLayerListArgs {}

/// Lists all layers configured in the repository.
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
/// ## Layer Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LayerEntry`](crate::interface::LoreEvent::LayerEntry) | Emitted for each layer configured in the repository |
pub async fn layer_list(
    globals: LoreGlobalArgs,
    args: LoreLayerListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, layer_list_local).await
}

async fn layer_list_local(
    globals: LoreGlobalArgs,
    args: LoreLayerListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        layer_list,
        move |repository, _args| list::list(repository),
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(layer_list_staged_local)]
/// Arguments for listing configured layers that have staged changes (no parameters).
pub struct LoreLayerListStagedArgs {}

/// Discovers configured layers with staged changes and emits a
/// `LayerStagedEntry` event per layer with `(target_path, source_repository,
/// staged_file_count)`. Used by the CLI to drive the per-layer commit-message
/// prompt.
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
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` |
///
/// ## Layer Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LayerStagedEntry`](crate::interface::LoreEvent::LayerStagedEntry) | One per layer with staged changes |
pub async fn layer_list_staged(
    globals: LoreGlobalArgs,
    args: LoreLayerListStagedArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, layer_list_staged_local).await
}

async fn layer_list_staged_local(
    globals: LoreGlobalArgs,
    args: LoreLayerListStagedArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        layer_list_staged,
        move |repository, _args| async move {
            lore_revision::layer::list_staged(repository).await?;
            Ok::<(), LayerError>(())
        },
    )
    .await
}
