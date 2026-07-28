// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::link::LinkError;
pub use lore_revision::link::LinkFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreString;

/// Arguments for adding a new link to a linked repository at the given path.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(add_local)]
pub struct LoreLinkAddArgs {
    /// Link repository URL
    pub link: LoreString,
    /// Path within this repository where the link is added
    pub link_path: LoreString,
    /// Source path within the linked repository; `/` or `\` means the root
    pub source_path: LoreString,
    /// Branch or revision to set the link pin at
    pub pin: LoreString,
    /// Disable automatic branch creation in the linked repository
    pub disable_branching: u8,
}

/// Adds a new link to a linked repository at the specified path.
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
/// ## Link Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::RepositoryCloneBegin`](crate::interface::LoreEvent::RepositoryCloneBegin) | Emitted when cloning a linked repository begins |
/// | [`LoreEvent::RepositoryCloneEnd`](crate::interface::LoreEvent::RepositoryCloneEnd) | Emitted when cloning a linked repository completes |
/// | [`LoreEvent::LinkChange`](crate::interface::LoreEvent::LinkChange) | Emitted when the link has been added and saved |
pub async fn add(
    globals: LoreGlobalArgs,
    args: LoreLinkAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, add_local).await
}

async fn add_local(
    globals: LoreGlobalArgs,
    args: LoreLinkAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        add,
        |repository, token, args| async move { add_impl(repository, &token, args).await },
    )
    .await
}

async fn add_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreLinkAddArgs,
) -> Result<(), LinkError> {
    let link_identifier = args.link.to_string();
    let link_path =
        RelativePath::new_from_user_path(repository.require_path()?, args.link_path.as_str())
            .forward::<LinkError>("resolving link path")?;
    let source_path = RelativePath::new_from_initial_path(
        if args.source_path.as_str() == "/" || args.source_path.as_str() == "\\" {
            ""
        } else {
            args.source_path.as_str()
        },
    )
    .forward::<LinkError>("resolving link source path")?;

    lore_revision::link::add::add(
        repository,
        token,
        link_path,
        link_identifier,
        source_path,
        args.pin.into(),
        args.disable_branching != 0,
    )
    .await
}

/// Arguments for removing a link from the repository at the given path.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(remove_local)]
pub struct LoreLinkRemoveArgs {
    /// Path within this repository where the link is removed
    pub link_path: LoreString,
}

/// Removes a link from the repository at the specified path.
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
/// ## Link Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LinkChange`](crate::interface::LoreEvent::LinkChange) | Emitted when the link has been removed |
pub async fn remove(
    globals: LoreGlobalArgs,
    args: LoreLinkRemoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, remove_local).await
}

async fn remove_local(
    globals: LoreGlobalArgs,
    args: LoreLinkRemoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        remove,
        |repository, token, args| async move { remove_impl(repository, &token, args).await },
    )
    .await
}

async fn remove_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreLinkRemoveArgs,
) -> Result<(), LinkError> {
    let link_path =
        RelativePath::new_from_user_path(repository.require_path()?, args.link_path.as_str())
            .forward::<LinkError>("resolving link path")?;

    lore_revision::link::remove::remove(repository, token, link_path).await
}

/// Arguments for listing all linked repositories in the current repository.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(list_local)]
pub struct LoreLinkListArgs {}

/// Lists all linked repositories registered in the current repository.
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
/// ## Link Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LinkEntry`](crate::interface::LoreEvent::LinkEntry) | Emitted for each linked repository |
pub async fn list(
    globals: LoreGlobalArgs,
    args: LoreLinkListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, list_local).await
}

async fn list_local(
    globals: LoreGlobalArgs,
    args: LoreLinkListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, list, move |repository, _args| {
        lore_revision::link::list::list(repository)
    })
    .await
}

pub async fn list_staged(globals: LoreGlobalArgs, callback: LoreEventCallback) -> i32 {
    repository_call_read(
        globals,
        callback,
        (),
        list_staged,
        move |repository, _args| lore_revision::link::list::list_staged(repository),
    )
    .await
}

/// Arguments for updating the pin or properties of an existing link.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(update_local)]
pub struct LoreLinkUpdateArgs {
    /// Path within this repository of the link to update
    pub link_path: LoreString,
    /// Branch or specific revision to pin the link to
    pub pin: LoreString,
}

/// Updates the pin or other properties of an existing link.
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
/// ## Link Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LinkChange`](crate::interface::LoreEvent::LinkChange) | Emitted when a link property is updated and again when the link update is finalized |
pub async fn update(
    globals: LoreGlobalArgs,
    args: LoreLinkUpdateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, update_local).await
}

async fn update_local(
    globals: LoreGlobalArgs,
    args: LoreLinkUpdateArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        update,
        |repository, token, args| async move { update_impl(repository, &token, args).await },
    )
    .await
}

async fn update_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreLinkUpdateArgs,
) -> Result<(), LinkError> {
    let link_path =
        RelativePath::new_from_user_path(repository.require_path()?, args.link_path.as_str())
            .forward::<LinkError>("resolving link path")?;

    lore_revision::link::update::update(repository, token, link_path, args.pin.into()).await
}
