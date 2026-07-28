// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::branch::BranchError;
use lore_revision::branch::latest::ListOptions;
use lore_revision::branch::merge::MergeError;
use lore_revision::branch::merge::MergeIntoOptions;
use lore_revision::branch::merge::MergeScope;
use lore_revision::branch::merge::MergeStartOptions;
use lore_revision::branch::push::PushOptions;
use lore_revision::branch::reset::ResetError;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreMetadataType;
use lore_revision::lore::BranchId;
use lore_revision::lore::execution_context;
use lore_revision::lore_debug;
use lore_revision::lore_error;
use lore_revision::metadata::branch::BranchMetadataError;
use lore_revision::repository;
use lore_revision::repository::BranchSwitchOptions;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call_delegation::dispatch_call;
use crate::interface::Context;
use crate::interface::LoreString;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(create_local)]
/// Arguments for creating a new branch with the given name and category.
pub struct LoreBranchCreateArgs {
    /// Name of the branch
    pub branch: LoreString,
    /// Category of the branch
    pub category: LoreString,
    /// Optional explicit branch ID (hex-encoded 16-byte context)
    pub id: LoreString,
}

/// Creates a new branch with the given name and category.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchCreate`](crate::interface::LoreEvent::BranchCreate) | Emitted when the branch has been successfully created, includes branch name and id |
pub async fn create(
    globals: LoreGlobalArgs,
    args: LoreBranchCreateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, create_local).await
}

async fn create_local(
    globals: LoreGlobalArgs,
    args: LoreBranchCreateArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        create,
        move |repository, token, args| async move {
            let branch = args.branch.to_string();
            let category = args.category.to_string();
            let id: Option<&str> = (&args.id).into();
            let id = id.and_then(|s| s.parse().ok());

            lore_revision::branch::create::create(
                repository,
                &token,
                branch,
                id,
                category,
                execution_context().globals().force(),
            )
            .await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(info_local)]
/// Arguments for retrieving branch metadata (name, id, category, protection status).
pub struct LoreBranchInfoArgs {
    /// Name of the branch
    pub branch: LoreString,
}

/// Retrieves metadata for a branch including its name, id, category, and protection status.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchInfo`](crate::interface::LoreEvent::BranchInfo) | Emitted with branch metadata (name, id, category, protection status, etc.) |
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreBranchInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_local).await
}

async fn info_local(
    globals: LoreGlobalArgs,
    args: LoreBranchInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, info, move |repository, args| {
        let branch_name = args.branch.to_string();

        lore_revision::branch::info::info(repository, branch_name)
    })
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(diff_local)]
/// Arguments for diffing two branches, reporting changed and conflicting files.
pub struct LoreBranchDiffArgs {
    /// Source branch name
    pub source: LoreString,
    /// Target branch name
    pub target: LoreString,
    /// Optional path in the repository to limit the diff to
    pub path: LoreString,
    /// Attempt to auto resolve conflicts
    pub auto_resolve: u8,
}

/// Computes the diff between two branches, reporting changed and conflicting files.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchDiffBegin`](crate::interface::LoreEvent::BranchDiffBegin) | Emitted before diff results begin streaming |
/// | [`LoreEvent::BranchDiffChangeBegin`](crate::interface::LoreEvent::BranchDiffChangeBegin) | Emitted before the list of changed files begins |
/// | [`LoreEvent::BranchDiffChange`](crate::interface::LoreEvent::BranchDiffChange) | Emitted for each changed file between the two branches |
/// | [`LoreEvent::BranchDiffChangeEnd`](crate::interface::LoreEvent::BranchDiffChangeEnd) | Emitted after all changed files have been reported |
/// | [`LoreEvent::BranchDiffConflictBegin`](crate::interface::LoreEvent::BranchDiffConflictBegin) | Emitted before the list of conflicting files begins |
/// | [`LoreEvent::BranchDiffConflict`](crate::interface::LoreEvent::BranchDiffConflict) | Emitted for each file that has a conflict between the two branches |
/// | [`LoreEvent::BranchDiffConflictEnd`](crate::interface::LoreEvent::BranchDiffConflictEnd) | Emitted after all conflict files have been reported |
/// | [`LoreEvent::BranchDiffEnd`](crate::interface::LoreEvent::BranchDiffEnd) | Emitted after all diff results have been streamed |
pub async fn diff(
    globals: LoreGlobalArgs,
    args: LoreBranchDiffArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, diff_local).await
}

async fn diff_local(
    globals: LoreGlobalArgs,
    args: LoreBranchDiffArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, diff, move |repository, args| {
        lore_revision::branch::diff::diff(
            repository,
            args.source.to_string(),
            args.target.to_string(),
            args.path.to_string(),
            args.auto_resolve != 0,
        )
    })
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(list_local)]
/// Arguments for listing all branches in the repository.
pub struct LoreBranchListArgs {
    /// Include archived local branches in listing
    pub archived: u8,
}

/// Lists all branches in the repository.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchListBegin`](crate::interface::LoreEvent::BranchListBegin) | Emitted before branch list entries begin streaming |
/// | [`LoreEvent::BranchListEntry`](crate::interface::LoreEvent::BranchListEntry) | Emitted for each branch in the repository |
/// | [`LoreEvent::BranchListEnd`](crate::interface::LoreEvent::BranchListEnd) | Emitted after all branch entries have been streamed |
pub async fn list(
    globals: LoreGlobalArgs,
    args: LoreBranchListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, list_local).await
}

async fn list_local(
    globals: LoreGlobalArgs,
    args: LoreBranchListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_read(globals, callback, args, list, move |repository, args| {
        branch::list_output(
            repository,
            execution_context().globals().local(),
            execution_context().globals().remote(),
            args.archived != 0,
        )
    })
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_start_local)]
/// Arguments for merging a source branch into the current branch.
pub struct LoreBranchMergeStartArgs {
    /// Name of the source branch to merge into the current branch
    pub branch: LoreString,
    /// Message to use for an auto commit if no conflicts arise
    pub message: LoreString,
    /// Disable auto commit even if no conflicts arise
    pub no_commit: u8,
    /// Optional link path for link-scoped merge
    pub link: LoreString,
    /// Merge only the main repository, skipping all linked repositories
    pub ignore_links: u8,
}

/// Begins merging a source branch into the current branch, auto-committing if there are no conflicts.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeStartBegin`](crate::interface::LoreEvent::BranchMergeStartBegin) | Emitted when merge begins, includes source branch and revision info |
/// | [`LoreEvent::BranchMergeStartEnd`](crate::interface::LoreEvent::BranchMergeStartEnd) | Emitted when merge operation completes, includes sync stats and conflict flag |
/// | [`LoreEvent::BranchMergeConflictFile`](crate::interface::LoreEvent::BranchMergeConflictFile) | Emitted for each file with an unresolved merge conflict |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during the apply_diff phase of the merge |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file modified during merge realization |
/// | [`LoreEvent::FileStageFile`](crate::interface::LoreEvent::FileStageFile) | Emitted for each file staged for deletion during merge realization |
/// | [`LoreEvent::RevisionCommitBegin`](crate::interface::LoreEvent::RevisionCommitBegin) | Emitted when auto-commit starts (no conflicts, no_commit=false) |
/// | [`LoreEvent::RevisionCommitProgress`](crate::interface::LoreEvent::RevisionCommitProgress) | Emitted periodically during auto-commit |
/// | [`LoreEvent::RevisionCommitEnd`](crate::interface::LoreEvent::RevisionCommitEnd) | Emitted when auto-commit file processing completes |
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the committed revision details |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the committed revision |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each fragment written during auto-commit |
pub async fn merge_start(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_start_local).await
}

async fn merge_start_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_start,
        move |repository, token, args| {
            let link_str = args.link.to_string();
            let scope = if args.ignore_links != 0 {
                MergeScope::MainOnly
            } else if link_str.is_empty() {
                MergeScope::All
            } else {
                MergeScope::Link(link_str)
            };

            let options = MergeStartOptions {
                message: args.message.to_string(),
                no_commit: args.no_commit != 0,
                scope,
            };

            async move {
                let branch = branch::resolve(repository.clone(), args.branch.as_str())
                    .await
                    .forward::<MergeError>("resolving branch")?;

                branch::merge::merge_start(repository, &token, branch.id, options).await
            }
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_abort_local)]
/// Arguments for aborting an in-progress branch merge.
pub struct LoreBranchMergeAbortArgs {
    /// Optional link path for link-scoped abort
    pub link: LoreString,
    /// Abort only the main repository merge, keeping link pin updates
    pub ignore_links: u8,
}

/// Aborts an in-progress branch merge, reverting the working directory to its pre-merge state.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeAbortBegin`](crate::interface::LoreEvent::BranchMergeAbortBegin) | Emitted when aborting a branch merge, includes staged and current revision hashes |
/// | [`LoreEvent::BranchMergeAbortEnd`](crate::interface::LoreEvent::BranchMergeAbortEnd) | Emitted after the merge abort has been completed |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization while reverting merge changes |
pub async fn merge_abort(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeAbortArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_abort_local).await
}

async fn merge_abort_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeAbortArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_abort,
        move |repository, token, args| {
            let link_str = args.link.to_string();
            let link = if link_str.is_empty() {
                None
            } else {
                Some(link_str)
            };
            let ignore_links = args.ignore_links != 0;
            async move {
                branch::merge::branch_merge_abort(repository, &token, link, ignore_links).await
            }
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_unresolve_local)]
/// Arguments for marking resolved merge paths as unresolved again.
pub struct LoreBranchMergeUnresolveArgs {
    /// Paths to mark unresolved
    pub paths: LoreArray<LoreString>,
}

/// Marks previously resolved merge paths as unresolved, restoring their conflict state.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeUnresolveFile`](crate::interface::LoreEvent::BranchMergeUnresolveFile) | Emitted for each file that was marked as unresolved |
/// | [`LoreEvent::BranchMergeUnresolveRevision`](crate::interface::LoreEvent::BranchMergeUnresolveRevision) | Emitted with the updated staged revision after unresolve completes |
pub async fn merge_unresolve(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeUnresolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_unresolve_local).await
}

async fn merge_unresolve_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeUnresolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_unresolve,
        move |repository, token, args| async move {
            branch::merge::branch_merge_unresolve(repository, &token, args.paths).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_into_local)]
/// Arguments for merging the current branch's staged changes into a target branch.
pub struct LoreBranchMergeIntoArgs {
    /// Name of the target branch to merge into
    pub branch: LoreString,
    /// ID of the target branch to merge into
    pub branch_id: Context,
    /// Commit message for the auto-commit
    pub message: LoreString,
    /// Optional link path for link-scoped merge into
    pub link: LoreString,
    /// Merge only the main repository, skipping all linked repositories
    pub ignore_links: u8,
}

/// Merges the current branch's staged changes into a target branch and auto-commits if conflict-free.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeIntoFileBegin`](crate::interface::LoreEvent::BranchMergeIntoFileBegin) | Emitted when starting to merge files into the target branch |
/// | [`LoreEvent::BranchMergeIntoFile`](crate::interface::LoreEvent::BranchMergeIntoFile) | Emitted for each file being merged into the target branch |
/// | [`LoreEvent::BranchMergeIntoFileEnd`](crate::interface::LoreEvent::BranchMergeIntoFileEnd) | Emitted after all files have been merged |
/// | [`LoreEvent::BranchMergeIntoFragmentBegin`](crate::interface::LoreEvent::BranchMergeIntoFragmentBegin) | Emitted when starting fragment transfer for a file |
/// | [`LoreEvent::BranchMergeIntoFragmentProgress`](crate::interface::LoreEvent::BranchMergeIntoFragmentProgress) | Emitted periodically during fragment transfer |
/// | [`LoreEvent::BranchMergeIntoFragmentEnd`](crate::interface::LoreEvent::BranchMergeIntoFragmentEnd) | Emitted when fragment transfer for a file completes |
/// | [`LoreEvent::BranchMergeIntoRevision`](crate::interface::LoreEvent::BranchMergeIntoRevision) | Emitted with the resulting revision after the merge into is complete |
/// | [`LoreEvent::RevisionCommitBegin`](crate::interface::LoreEvent::RevisionCommitBegin) | Emitted when auto-commit starts (if no conflicts) |
/// | [`LoreEvent::RevisionCommitProgress`](crate::interface::LoreEvent::RevisionCommitProgress) | Emitted periodically during auto-commit file processing |
/// | [`LoreEvent::RevisionCommitEnd`](crate::interface::LoreEvent::RevisionCommitEnd) | Emitted when auto-commit file processing completes |
/// | [`LoreEvent::RevisionCommitRevision`](crate::interface::LoreEvent::RevisionCommitRevision) | Emitted with the committed revision details |
/// | [`LoreEvent::Metadata`](crate::interface::LoreEvent::Metadata) | Emitted for each metadata entry of the committed revision |
/// | [`LoreEvent::FragmentWrite`](crate::interface::LoreEvent::FragmentWrite) | Emitted for each file fragment written or deduplicated during commit |
pub async fn merge_into(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeIntoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_into_local).await
}

async fn merge_into_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeIntoArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_into,
        move |repository, token, args| {
            let link_str = args.link.to_string();
            let ignore_links = args.ignore_links != 0;
            let options = MergeIntoOptions {
                message: args.message.to_string(),
                link: if ignore_links || link_str.is_empty() {
                    None
                } else {
                    Some(link_str)
                },
                ignore_links,
            };

            async move {
                let branch = branch::resolve(repository.clone(), args.branch.as_str())
                    .await
                    .forward::<MergeError>("resolving branch")?;

                branch::merge::merge_into(repository, &token, branch.id, options).await
            }
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_restart_local)]
/// Arguments for re-applying merge conflict resolution for the given paths.
pub struct LoreBranchMergeRestartArgs {
    /// Paths to re-materialize
    pub paths: LoreArray<LoreString>,
}

/// Re-applies merge conflict resolution for specified paths, re-materializing their working copies.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeConflictFile`](crate::interface::LoreEvent::BranchMergeConflictFile) | Emitted for each file with a remaining merge conflict |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization during restart |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file re-materialized during restart |
pub async fn merge_restart(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeRestartArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_restart_local).await
}

async fn merge_restart_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeRestartArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_restart,
        move |repository, token, args| async move {
            branch::merge::merge_restart(repository, &token, args.paths).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_resolve_local)]
/// Arguments for marking conflicted paths as resolved.
pub struct LoreBranchMergeResolveArgs {
    /// Paths to mark resolved
    pub paths: LoreArray<LoreString>,
}

/// Marks specified conflicted paths as resolved in the current branch merge.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeResolveFile`](crate::interface::LoreEvent::BranchMergeResolveFile) | Emitted for each file that was marked as resolved |
/// | [`LoreEvent::BranchMergeResolveRevision`](crate::interface::LoreEvent::BranchMergeResolveRevision) | Emitted with the updated staged revision after resolve completes |
pub async fn merge_resolve(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_resolve_local).await
}

async fn merge_resolve_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_resolve,
        move |repository, token, args| async move {
            branch::merge::branch_merge_resolve(repository, &token, args.paths).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_resolve_mine_local)]
/// Arguments for resolving conflicts by accepting the local ("mine") version.
pub struct LoreBranchMergeResolveMineArgs {
    /// Paths to resolve as "mine"
    pub paths: LoreArray<LoreString>,
}

/// Resolves merge conflicts for specified paths by accepting the local ("mine") version.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeResolveFile`](crate::interface::LoreEvent::BranchMergeResolveFile) | Emitted for each file resolved by keeping "mine" |
/// | [`LoreEvent::BranchMergeResolveRevision`](crate::interface::LoreEvent::BranchMergeResolveRevision) | Emitted with the updated staged revision |
pub async fn merge_resolve_mine(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeResolveMineArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_resolve_mine_local).await
}

async fn merge_resolve_mine_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeResolveMineArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_resolve_mine,
        move |repository, token, args| async move {
            branch::merge::merge_resolve_mine(repository, &token, args.paths).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(merge_resolve_theirs_local)]
/// Arguments for resolving conflicts by accepting the incoming ("theirs") version.
pub struct LoreBranchMergeResolveTheirsArgs {
    /// Paths to resolve as "theirs"
    pub paths: LoreArray<LoreString>,
}

/// Resolves merge conflicts for specified paths by accepting the incoming ("theirs") version.
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
/// ## Merge Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchMergeResolveFile`](crate::interface::LoreEvent::BranchMergeResolveFile) | Emitted for each file resolved by keeping "theirs" |
/// | [`LoreEvent::BranchMergeResolveRevision`](crate::interface::LoreEvent::BranchMergeResolveRevision) | Emitted with the updated staged revision |
pub async fn merge_resolve_theirs(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeResolveTheirsArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, merge_resolve_theirs_local).await
}

async fn merge_resolve_theirs_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMergeResolveTheirsArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        merge_resolve_theirs,
        move |repository, token, args| async move {
            branch::merge::merge_resolve_theirs(repository, &token, args.paths).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(push_local)]
/// Arguments for pushing a branch and its revisions to the remote.
pub struct LoreBranchPushArgs {
    /// Optional branch to push, current branch if not given
    pub branch: LoreString,
    /// Allow the server to fast-forward merge if the target branch head has moved
    pub fast_forward_merge: u8,
}

/// Pushes the current or specified branch and its revisions to the remote.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchPush`](crate::interface::LoreEvent::BranchPush) | Emitted when push begins, includes branch name and revision info |
/// | [`LoreEvent::BranchPushBranchCreateBegin`](crate::interface::LoreEvent::BranchPushBranchCreateBegin) | Emitted when creating the remote branch (first push) |
/// | [`LoreEvent::BranchPushBranchCreateEnd`](crate::interface::LoreEvent::BranchPushBranchCreateEnd) | Emitted when remote branch creation completes |
/// | [`LoreEvent::BranchPushRevisionUpdateBegin`](crate::interface::LoreEvent::BranchPushRevisionUpdateBegin) | Emitted when updating a revision on the remote |
/// | [`LoreEvent::BranchPushRevisionUpdateEnd`](crate::interface::LoreEvent::BranchPushRevisionUpdateEnd) | Emitted when a revision update completes |
/// | [`LoreEvent::BranchPushFragmentBegin`](crate::interface::LoreEvent::BranchPushFragmentBegin) | Emitted when uploading fragment data begins |
/// | [`LoreEvent::BranchPushFragmentProgress`](crate::interface::LoreEvent::BranchPushFragmentProgress) | Emitted periodically during fragment upload |
/// | [`LoreEvent::BranchPushFragmentEnd`](crate::interface::LoreEvent::BranchPushFragmentEnd) | Emitted when fragment upload completes |
/// | [`LoreEvent::BranchPushRevisionPushBegin`](crate::interface::LoreEvent::BranchPushRevisionPushBegin) | Emitted when pushing a revision to the remote begins |
/// | [`LoreEvent::BranchPushRevisionPushUpdate`](crate::interface::LoreEvent::BranchPushRevisionPushUpdate) | Emitted with progress updates during revision push |
/// | [`LoreEvent::BranchPushRevisionPushEnd`](crate::interface::LoreEvent::BranchPushRevisionPushEnd) | Emitted when revision push completes |
pub async fn push(
    globals: LoreGlobalArgs,
    args: LoreBranchPushArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, push_local).await
}

async fn push_local(
    globals: LoreGlobalArgs,
    args: LoreBranchPushArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        push,
        |repository, token, args| async move { push_impl(repository, &token, args).await },
    )
    .await
}

async fn push_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreBranchPushArgs,
) -> Result<(), branch::push::PushError> {
    repository
        .remote()
        .await
        .forward::<branch::push::PushError>("acquiring remote")?;

    let options = PushOptions {
        branch: args.branch.into(),
        fast_forward_merge: args.fast_forward_merge != 0,
    };

    // Push is never local
    repository.set_disable_upload(false);

    lore_revision::branch::push::push(repository, token, options).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(switch_local)]
/// Arguments for switching the working directory to a different branch or revision.
pub struct LoreBranchSwitchArgs {
    /// Name of the branch
    pub branch: LoreString,
    /// Hash of the revision
    pub revision: LoreString,
    /// Reset local modified files to match the incoming revision
    pub reset: u8,
    /// Only update anchor tracking without modifying or verifying files
    pub bare: u8,
}

/// Switches the working directory to a different branch or revision.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchSwitchBegin`](crate::interface::LoreEvent::BranchSwitchBegin) | Emitted when branch switch starts |
/// | [`LoreEvent::BranchSwitchEnd`](crate::interface::LoreEvent::BranchSwitchEnd) | Emitted when branch switch completes successfully |
/// | [`LoreEvent::RevisionSyncTarget`](crate::interface::LoreEvent::RevisionSyncTarget) | Emitted with target revision info after resolving the switch target |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file modified/added/deleted during switch |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted periodically during file realization |
/// | [`LoreEvent::RevisionSyncRevision`](crate::interface::LoreEvent::RevisionSyncRevision) | Emitted with the resulting revision after switch |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by view or ignore filters |
/// | [`LoreEvent::RevisionResolve`](crate::interface::LoreEvent::RevisionResolve) | Emitted when resolving a partial revision reference |
pub async fn switch(
    globals: LoreGlobalArgs,
    args: LoreBranchSwitchArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, switch_local).await
}

async fn switch_local(
    globals: LoreGlobalArgs,
    args: LoreBranchSwitchArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        switch,
        move |repository, token, args| async move {
            let context = execution_context();
            let call = context.globals();
            let branch = args.branch.to_string();

            let options = BranchSwitchOptions {
                signature: args.revision.into(),
                local: call.local() || call.offline(),
                reset: args.reset != 0,
                search_nearest: call.search_nearest(),
                bare: args.bare != 0,
            };

            repository::branch_switch(repository, &token, branch, options).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(protect_local)]
/// Arguments for applying write protection to a branch.
pub struct LoreBranchProtectArgs {
    /// Name of the branch
    pub branch: LoreString,
}

/// Applies write protection to a branch.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchProtect`](crate::interface::LoreEvent::BranchProtect) | Emitted when the branch has been successfully protected |
pub async fn protect(
    globals: LoreGlobalArgs,
    args: LoreBranchProtectArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, protect_local).await
}

async fn protect_local(
    globals: LoreGlobalArgs,
    args: LoreBranchProtectArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        protect,
        move |repository, _token, args| async move {
            let branch = branch::resolve(repository.clone(), args.branch.as_str()).await?;

            branch::protect(repository, branch.id).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(unprotect_local)]
/// Arguments for removing write protection from a branch.
pub struct LoreBranchUnprotectArgs {
    /// Name of the branch
    pub branch: LoreString,
}

/// Removes write protection from a branch.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchUnprotect`](crate::interface::LoreEvent::BranchUnprotect) | Emitted when the branch has been successfully unprotected |
pub async fn unprotect(
    globals: LoreGlobalArgs,
    args: LoreBranchUnprotectArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, unprotect_local).await
}

async fn unprotect_local(
    globals: LoreGlobalArgs,
    args: LoreBranchUnprotectArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        unprotect,
        move |repository, _token, args| async move {
            let branch = branch::resolve(repository.clone(), args.branch.as_str()).await?;

            branch::unprotect(repository, branch.id).await
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(archive_local)]
/// Arguments for archiving a branch locally and (unless local mode) on the remote.
pub struct LoreBranchArchiveArgs {
    /// Name of the branch
    pub branch: LoreString,
}

/// Archives a branch locally and, unless running in local mode, on the remote.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchArchive`](crate::interface::LoreEvent::BranchArchive) | Emitted when the branch has been successfully archived |
pub async fn archive(
    globals: LoreGlobalArgs,
    args: LoreBranchArchiveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, archive_local).await
}

async fn archive_local(
    globals: LoreGlobalArgs,
    args: LoreBranchArchiveArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        archive,
        |repository, _token, args| archive_impl(repository, args),
    )
    .await
}

async fn archive_impl(
    repository: Arc<RepositoryContext>,
    args: LoreBranchArchiveArgs,
) -> Result<(), BranchError> {
    let execution = execution_context();

    let branch = branch::resolve(repository.clone(), args.branch.as_str()).await?;

    let mut local_fail = false;

    // Make sure branch is not current
    let mut local_current = false;
    if let Ok((_revision, current_branch)) =
        lore_revision::instance::load_current_anchor(&repository).await
        && current_branch == branch.id
    {
        lore_error!("Cannot archive the current branch");
        execution
            .dispatcher
            .send_error(BranchError::from(lore_base::error::DeleteCurrent {
                branch: branch.id.to_string(),
            }));
        local_fail = true;
        local_current = true;
    }

    if !local_fail {
        // Archive branch
        lore_debug!("Attempt archive of local branch");
        if let Err(err) = branch::delete(repository.clone(), branch.id).await {
            execution.dispatcher.send_error(err);
            local_fail = true;
        }
    }

    if !local_current
        && !execution_context().globals().local()
        && let Ok(remote) = repository.remote().await
    {
        // Archive remote branch
        lore_debug!("Attempt archive of remote branch");
        if let Err(err) = branch::delete_remote(remote.clone(), repository.id, branch.id).await {
            execution.dispatcher.send_error(err);
        }
    }

    if local_fail {
        return Err(BranchError::from(lore_base::error::BranchNotFound {
            branch: branch.id.to_string(),
        }));
    }

    Ok(())
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(reset_local)]
/// Arguments for resetting a branch's local LATEST pointer to a specific revision.
pub struct LoreBranchResetArgs {
    /// Revision to reset the local LATEST pointer to
    pub revision: LoreString,
    /// Branch to reset, current branch if empty
    pub branch: LoreString,
}

/// Resets the local LATEST pointer of a branch to a specific revision.
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
/// ## Branch Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::BranchReset`](crate::interface::LoreEvent::BranchReset) | Emitted when the branch has been reset to the target revision |
pub async fn reset(
    globals: LoreGlobalArgs,
    args: LoreBranchResetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, reset_local).await
}

async fn reset_local(
    globals: LoreGlobalArgs,
    args: LoreBranchResetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        reset,
        |repository, token, args| async move { reset_impl(repository, &token, args).await },
    )
    .await
}

async fn reset_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreBranchResetArgs,
) -> Result<(), ResetError> {
    branch::reset::reset(
        repository,
        token,
        args.branch.to_string(),
        args.revision.to_string(),
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Arguments for listing a branch's LATEST revision history.
pub struct LoreBranchLatestListArgs {
    /// Branch to list, current branch if empty
    pub branch: LoreString,
    /// Maximum entries to return (`0` uses the default of 30)
    pub limit: u32,
}

pub async fn latest_list(
    globals: LoreGlobalArgs,
    args: LoreBranchLatestListArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        latest_list,
        |repository, _token, args| latest_list_impl(repository, args),
    )
    .await
}

async fn latest_list_impl(
    repository: Arc<RepositoryContext>,
    args: LoreBranchLatestListArgs,
) -> Result<(), BranchError> {
    let options = ListOptions {
        branch: args.branch.into(),
        limit: args.limit,
    };

    branch::latest::list(repository, options).await
}

// --- Branch metadata commands ---

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_get_local)]
/// Arguments for retrieving branch metadata (one key or all).
pub struct LoreBranchMetadataGetArgs {
    /// Branch name or identifier
    pub branch: LoreString,
    /// Metadata key (empty string lists all)
    pub key: LoreString,
}

/// Resolve a branch name or identifier to its id, defaulting to the current branch when the
/// name is empty. Branch metadata commands accept an optional branch and operate on the
/// instance's current branch when none is given.
async fn resolve_branch_id_or_current(
    repository: Arc<RepositoryContext>,
    branch: &str,
) -> Result<BranchId, BranchMetadataError> {
    if branch.is_empty() {
        let (_revision, current_branch) = lore_revision::instance::load_current_anchor(&repository)
            .await
            .map_err(|_err| lore_base::error::InvalidArguments {
                reason: "no current branch to operate on; specify --branch".into(),
            })?;
        return Ok(current_branch);
    }

    let status = branch::resolve(repository, branch).await.map_err(|_err| {
        lore_base::error::InvalidArguments {
            reason: format!("branch '{branch}' not found"),
        }
    })?;
    Ok(status.id)
}

/// Retrieves branch metadata. If `key` is non-empty, returns that single key's value.
/// If `key` is empty, returns all metadata entries.
pub async fn metadata_get(
    globals: LoreGlobalArgs,
    args: LoreBranchMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_get_local).await
}

async fn metadata_get_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_get,
        move |repository, _token, args| {
            let branch_name = args.branch.to_string();
            let key = if args.key.is_empty() {
                None
            } else {
                Some(args.key.to_string())
            };
            async move {
                let branch_id =
                    resolve_branch_id_or_current(repository.clone(), &branch_name).await?;

                lore_revision::metadata::branch::get(
                    repository,
                    branch_id,
                    key.as_deref(),
                    execution_context().globals().local(),
                )
                .await
            }
        },
    )
    .await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_set_local)]
/// Arguments for setting one or more key-value pairs on branch metadata.
pub struct LoreBranchMetadataSetArgs {
    /// Branch name or identifier
    pub branch: LoreString,
    /// Metadata keys to set (parallel with `values`/`formats`)
    pub keys: LoreArray<LoreString>,
    /// Values to set, one per key (decoded per the matching `formats` entry)
    pub values: LoreArray<LoreString>,
    /// Value type for each key, one per key
    pub formats: LoreArray<LoreMetadataType>,
}

/// Sets one or more metadata key-value pairs on the branch metadata.
pub async fn metadata_set(
    globals: LoreGlobalArgs,
    args: LoreBranchMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_set_local).await
}

async fn metadata_set_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMetadataSetArgs,
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
    args: LoreBranchMetadataSetArgs,
) -> Result<(), BranchMetadataError> {
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataType;

    let branch_id = resolve_branch_id_or_current(repository.clone(), args.branch.as_str()).await?;

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

    lore_revision::metadata::branch::set(repository, branch_id, &keys, &values, &formats).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(metadata_clear_local)]
/// Arguments for removing keys from branch metadata.
pub struct LoreBranchMetadataClearArgs {
    /// Branch name or identifier
    pub branch: LoreString,
    /// Keys to clear (empty array clears all user-defined keys)
    pub keys: LoreArray<LoreString>,
}

/// Removes metadata keys from the branch metadata.
pub async fn metadata_clear(
    globals: LoreGlobalArgs,
    args: LoreBranchMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_clear_local).await
}

async fn metadata_clear_local(
    globals: LoreGlobalArgs,
    args: LoreBranchMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_write(
        globals,
        callback,
        args,
        metadata_clear,
        move |repository, _token, args| {
            let branch_name = args.branch.to_string();
            let keys: Vec<String> = args.keys.as_slice().iter().map(|k| k.to_string()).collect();
            async move {
                let branch_id =
                    resolve_branch_id_or_current(repository.clone(), &branch_name).await?;

                let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
                lore_revision::metadata::branch::clear(repository, branch_id, &key_refs).await
            }
        },
    )
    .await
}
