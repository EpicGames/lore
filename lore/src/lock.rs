// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_macro::LoreArgs;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lock::file::acquire::AcquireOptions;
use lore_revision::lock::file::query::QueryOptions;
use lore_revision::lock::file::release::ReleaseOptions;
use lore_revision::lock::file::status::StatusOptions;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_read;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreString;

/// Arguments for acquiring file locks on the given paths for a branch.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(file_acquire_local)]
pub struct LoreLockFileAcquireArgs {
    /// Paths to acquire locks on
    pub paths: LoreArray<LoreString>,
    /// Branch the locks are acquired on
    pub branch: LoreString,
}

/// Acquires file locks on the specified paths for a given branch.
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
/// ## Lock Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LockFileAcquireBegin`](crate::interface::LoreEvent::LockFileAcquireBegin) | Emitted before each group of lock-acquire results |
/// | [`LoreEvent::LockFileAcquire`](crate::interface::LoreEvent::LockFileAcquire) | Emitted for each file related to the lock-acquired report |
pub async fn file_acquire(
    globals: LoreGlobalArgs,
    args: LoreLockFileAcquireArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, file_acquire_local).await
}

async fn file_acquire_local(
    globals: LoreGlobalArgs,
    args: LoreLockFileAcquireArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        file_acquire,
        move |repository, args| {
            let options: AcquireOptions = AcquireOptions {
                paths: args.paths,
                branch: args.branch.to_string(),
                owner: String::default(),
            };

            lore_revision::lock::file::acquire::acquire(repository, options)
        },
    )
    .await
}

pub async fn file_acquire_as_owner(
    globals: LoreGlobalArgs,
    args: LoreLockFileAcquireArgs,
    callback: LoreEventCallback,
    owner: LoreString,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        file_acquire,
        move |repository, args| {
            let options: AcquireOptions = AcquireOptions {
                paths: args.paths,
                branch: args.branch.to_string(),
                owner: owner.to_string(),
            };

            lore_revision::lock::file::acquire::acquire(repository, options)
        },
    )
    .await
}

/// Arguments for returning the lock status of the given files on a branch.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(file_status_local)]
pub struct LoreLockFileStatusArgs {
    /// Paths to get the lock status of
    pub paths: LoreArray<LoreString>,
    /// Branch the locks were acquired on
    pub branch: LoreString,
}

/// Returns the lock status of the specified files on a given branch.
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
/// ## Lock Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LockFileStatusBegin`](crate::interface::LoreEvent::LockFileStatusBegin) | Emitted before lock status results begin streaming |
/// | [`LoreEvent::LockFileStatus`](crate::interface::LoreEvent::LockFileStatus) | Emitted for each locked file with owner and lock details |
pub async fn file_status(
    globals: LoreGlobalArgs,
    args: LoreLockFileStatusArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, file_status_local).await
}

async fn file_status_local(
    globals: LoreGlobalArgs,
    args: LoreLockFileStatusArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        file_status,
        move |repository, args| {
            let options = StatusOptions {
                paths: args.paths,
                branch: args.branch.to_string(),
            };

            lore_revision::lock::file::status::status(repository, options)
        },
    )
    .await
}

/// Arguments for querying file locks on a branch, optionally filtered by owner and path.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(file_query_local)]
pub struct LoreLockFileQueryArgs {
    /// Branch to query locks on
    pub branch: LoreString,
    /// Owner filter; empty matches any owner
    pub owner: LoreString,
    /// Path filter; empty matches any path
    pub path: LoreString,
}

/// Queries file locks on a branch, optionally filtered by owner and path.
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
/// ## Lock Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LockFileQueryBegin`](crate::interface::LoreEvent::LockFileQueryBegin) | Emitted before query results begin streaming |
/// | [`LoreEvent::LockFileQuery`](crate::interface::LoreEvent::LockFileQuery) | Emitted for each file matching the query |
pub async fn file_query(
    globals: LoreGlobalArgs,
    args: LoreLockFileQueryArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, file_query_local).await
}

async fn file_query_local(
    globals: LoreGlobalArgs,
    args: LoreLockFileQueryArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        file_query,
        move |repository, args| {
            let options = QueryOptions {
                branch: args.branch.to_string(),
                owner: args.owner.to_string(),
                path: args.path.to_string(),
            };

            lore_revision::lock::file::query::query(repository, options)
        },
    )
    .await
}

/// Arguments for releasing file locks on the given paths for a branch and owner.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(file_release_local)]
pub struct LoreLockFileReleaseArgs {
    /// Paths to release locks on
    pub paths: LoreArray<LoreString>,
    /// Branch the locks were acquired on
    pub branch: LoreString,
    /// Owner of the lock
    pub owner: LoreString,
    /// Owner id of the lock
    pub owner_id: LoreString,
}

/// Releases file locks on the specified paths for a given branch and owner.
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
/// ## Lock Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::LockFileReleaseBegin`](crate::interface::LoreEvent::LockFileReleaseBegin) | Emitted before each group of lock-release results |
/// | [`LoreEvent::LockFileRelease`](crate::interface::LoreEvent::LockFileRelease) | Emitted for each file related to the lock-released report |
pub async fn file_release(
    globals: LoreGlobalArgs,
    args: LoreLockFileReleaseArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, file_release_local).await
}

async fn file_release_local(
    globals: LoreGlobalArgs,
    args: LoreLockFileReleaseArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(
        globals,
        callback,
        args,
        file_release,
        move |repository, args| {
            let options = ReleaseOptions {
                paths: args.paths,
                branch: args.branch.to_string(),
                owner: args.owner.to_string(),
                owner_id: args.owner_id.to_string(),
            };

            lore_revision::lock::file::release::release(repository, options)
        },
    )
    .await
}
