// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::LazyLock;

pub type LoreEvent = lore_revision::interface::LoreEvent;

/// Return the tag identifying the type of an event.
#[unsafe(no_mangle)]
pub extern "C" fn lore_event_type(event: &LoreEvent) -> u32 {
    event.discriminant()
}

pub type LoreLogConfig = crate::log::LoreLogConfig;
pub type LoreGlobalArgs = lore_revision::interface::LoreGlobalArgs;
pub type LoreEventCallback = lore_revision::interface::LoreEventCallback;
pub type LoreEventCallbackConfig = lore_revision::interface::LoreEventCallbackConfig;
pub type LoreError = lore_revision::interface::LoreError;
pub type LoreMetadata = lore_revision::interface::LoreMetadata;
pub type LoreMetadataType = lore_revision::interface::LoreMetadataType;
pub type LoreNodeType = lore_revision::interface::LoreNodeType;
pub type LoreFileAction = lore_revision::interface::LoreFileAction;
pub type LoreLogLevel = lore_base::log::LoreLogLevel;
pub type LoreBranchLocation = lore_revision::interface::LoreBranchLocation;

pub type Context = lore_storage::Context;
pub type Partition = lore_storage::Partition;
pub type Hash = lore_storage::Hash;
pub type Fragment = lore_storage::Fragment;
pub type FragmentFlags = lore_storage::FragmentFlags;
pub use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
pub type LoreString = lore_revision::interface::LoreString;
pub type LoreArray<T> = lore_revision::interface::LoreArray<T>;

/// Named by `lore_repository_create_args_t::use_shared_store`; re-exported so callers need no
/// dependency on `lore_revision`.
pub use lore_revision::repository::LoreSharedStoreMode;

use crate::call_delegation::run_asynchronously;
use crate::call_delegation::run_synchronously;
use crate::log;

pub type LoreErrorEventData = lore_revision::event::LoreErrorEventData;
pub type LoreLogEventData = lore_revision::event::LoreLogEventData;
pub type LoreMetadataEventData = lore_revision::event::LoreMetadataEventData;

pub type LoreFileStageProgressEventData = lore_revision::stage::LoreFileStageProgressEventData;
pub type LoreFileStageCountData = lore_revision::stage::LoreFileStageCountData;
pub type LoreFileResetProgressEventData =
    lore_revision::file::reset::LoreFileResetProgressEventData;
pub type LoreFileResetCountData = lore_revision::file::reset::LoreFileResetCountData;
pub type LoreFileUnstageCountData = lore_revision::file::unstage::LoreFileUnstageCountData;
pub type LorePathIgnoreEventData = lore_revision::path::LorePathIgnoreEventData;
pub type LoreRevisionHistoryEntryEventData =
    lore_revision::revision::history::LoreRevisionHistoryEntryEventData;
pub type LoreRevisionInfoEventData = lore_revision::revision::info::LoreRevisionInfoEventData;
pub type LoreRevisionInfoDeltaEventData =
    lore_revision::revision::info::LoreRevisionInfoDeltaEventData;
pub type LoreRevisionCommitRevisionEventData =
    lore_revision::commit::LoreRevisionCommitRevisionEventData;
pub type LoreRevisionCommitStatsEventData = lore_revision::commit::LoreRevisionCommitStatsEventData;
pub type LoreCommitFileStatsData = lore_revision::commit::LoreCommitFileStatsData;
pub type LoreBranchPushStatsEventData = lore_revision::branch::push::LoreBranchPushStatsEventData;
pub type LoreFragmentStatsData = lore_storage::FragmentWriteCounts;
pub type LoreRevisionSyncProgressEventData =
    lore_revision::revision::sync::LoreRevisionSyncProgressEventData;
pub type LoreRevisionSyncFileEventData =
    lore_revision::revision::sync::LoreRevisionSyncFileEventData;
pub type LoreRevisionSyncRevisionEventData =
    lore_revision::revision::sync::LoreRevisionSyncRevisionEventData;
pub type LoreRevisionBisectEventData = lore_revision::revision::bisect::LoreRevisionBisectEventData;
pub type LoreLinkChangeEventData = lore_revision::link::LoreLinkChangeEventData;
pub type LoreLinkEntryEventData = lore_revision::event::LoreLinkEntryEventData;
pub type LoreLinkBranchCreateEventData = lore_revision::link::LoreLinkBranchCreateEventData;
pub type LoreFragmentWriteEventData = lore_revision::immutable::LoreFragmentWriteEventData;
pub type LoreCompleteEventData = lore_revision::event::LoreCompleteEventData;
pub type LoreMaintenanceEventData = lore_revision::event::LoreMaintenanceEventData;

pub mod metadata {
    pub const MESSAGE: &str = lore_revision::metadata::MESSAGE;
    pub const TIMESTAMP: &str = lore_revision::metadata::TIMESTAMP;
    pub const CREATED_BY: &str = lore_revision::metadata::CREATED_BY;
    pub const COMMITTED_BY: &str = lore_revision::metadata::COMMITTED_BY;
    pub const REVIEWED_BY: &str = lore_revision::metadata::REVIEWED_BY;
    pub const MERGED_BY: &str = lore_revision::metadata::MERGED_BY;
    pub const BRANCH: &str = lore_revision::metadata::BRANCH;
    pub const P4_CHANGELIST: &str = lore_revision::metadata::P4_CHANGELIST;
    pub const RESTORED_FROM: &str = lore_revision::metadata::RESTORED_FROM;
    pub const REVERTED_FROM: &str = lore_revision::metadata::REVERTED_FROM;
}

pub type LoreAuthUserInfoArgs = crate::auth::LoreAuthUserInfoArgs;

/// Resolve user IDs to display names using the remote authentication service.
/// Requires an authenticated connection.
///
/// When no user IDs are provided, returns the current user's identity using
/// locally cached tokens (equivalent to `lore_auth_local_user_info`).
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Auth Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_AUTH_USER_INFO` | `lore_auth_user_info_event_data_t` | Emitted with user id and display name for each resolved user |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_user_info(
    globals: &LoreGlobalArgs,
    args: &LoreAuthUserInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::resolve_user_info)
}

/// Asynchronous version of `lore_auth_user_info`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_user_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthUserInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::resolve_user_info);
}

pub type LoreAuthLoginWithTokenArgs = crate::auth::LoreAuthLoginWithTokenArgs;

/// Authenticate using an existing bearer token.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Auth Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_AUTH_USER_INFO` | `lore_auth_user_info_event_data_t` | Emitted with user id and display name after successful token authentication |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_login_with_token(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLoginWithTokenArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::login_with_token)
}

/// Asynchronous version of `lore_auth_login_with_token`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_login_with_token_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLoginWithTokenArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::login_with_token);
}

pub type LoreAuthListArgs = crate::auth::LoreAuthListArgs;

/// List all stored authentication identities.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Auth Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_AUTH_IDENTITY` | `lore_auth_identity_event_data_t` | Emitted once per stored identity |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_list(
    globals: &LoreGlobalArgs,
    args: &LoreAuthListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::list)
}

/// Asynchronous version of `lore_auth_list`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::list);
}

pub type LoreAuthLogoutArgs = crate::auth::LoreAuthLogoutArgs;

/// Remove stored authentication and authorization tokens.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_logout(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLogoutArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::logout)
}

/// Asynchronous version of `lore_auth_logout`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_logout_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLogoutArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::logout);
}

pub type LoreAuthClearArgs = crate::auth::LoreAuthClearArgs;

/// Clear all stored authentication data.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_clear(
    globals: &LoreGlobalArgs,
    args: &LoreAuthClearArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::clear)
}

/// Asynchronous version of `lore_auth_clear`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_clear_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthClearArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::clear);
}

pub type LoreAuthLocalUserInfoArgs = crate::auth::LoreAuthLocalUserInfoArgs;

/// Resolve user identities to display names from locally stored JWT tokens.
///
/// Does not contact the auth service. Decodes cached JWT tokens to extract
/// display names. For user IDs without a local token, returns the raw user
/// ID. For remote resolution with proper authorization, use
/// `lore_auth_user_info` which queries the remote authentication service.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Auth Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_AUTH_USER_INFO` | `lore_auth_user_info_event_data_t` | Emitted with the resolved user id and display name |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_local_user_info(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLocalUserInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::local_user_info)
}

/// Asynchronous version of `lore_auth_local_user_info`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_local_user_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLocalUserInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::local_user_info);
}

pub type LoreAuthLoginInteractiveArgs = crate::auth::LoreAuthLoginInteractiveArgs;

/// Authenticate interactively via a browser-based login flow.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Auth Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_AUTH_URL` | `lore_auth_url_event_data_t` | Emitted with the login URL when no_browser mode is requested |
/// | `LORE_EVENT_AUTH_USER_INFO` | `lore_auth_user_info_event_data_t` | Emitted with user id and display name after successful interactive authentication |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_login_interactive(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLoginInteractiveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::auth::login_interactive)
}

/// Asynchronous version of `lore_auth_login_interactive`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Auth Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_AUTH_URL` | `lore_auth_url_event_data_t` | Emitted with the login URL when no_browser mode is requested |
/// | `LORE_EVENT_AUTH_USER_INFO` | `lore_auth_user_info_event_data_t` | Emitted with user id and display name after successful interactive authentication |
#[unsafe(no_mangle)]
pub extern "C" fn lore_auth_login_interactive_async(
    globals: &LoreGlobalArgs,
    args: &LoreAuthLoginInteractiveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::auth::login_interactive);
}

pub type LoreBranchCreateArgs = crate::branch::LoreBranchCreateArgs;

/// Create a new branch in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_CREATE` | `lore_branch_create_event_data_t` | Emitted when the branch has been successfully created, includes branch name and id |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_BRANCH_CREATE` | `lore_link_branch_create_event_data_t` | Emitted once per linked repository mount, reporting whether its branch was created or an existing one reused |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_create(
    globals: &LoreGlobalArgs,
    args: &LoreBranchCreateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::create)
}

/// Asynchronous version of `lore_branch_create`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_CREATE` | `lore_branch_create_event_data_t` | Emitted when the branch has been successfully created, includes branch name and id |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_BRANCH_CREATE` | `lore_link_branch_create_event_data_t` | Emitted once per linked repository mount, reporting whether its branch was created or an existing one reused |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_create_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchCreateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::create);
}

pub type LoreBranchInfoArgs = crate::branch::LoreBranchInfoArgs;

/// Retrieve metadata about a specific branch.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_INFO` | `lore_branch_info_event_data_t` | Emitted with branch metadata (name, id, category, protection status, etc.) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_info(
    globals: &LoreGlobalArgs,
    args: &LoreBranchInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::info)
}

/// Asynchronous version of `lore_branch_info`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_INFO` | `lore_branch_info_event_data_t` | Emitted with branch metadata (name, id, category, protection status, etc.) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::info);
}

pub type LoreBranchDiffArgs = crate::branch::LoreBranchDiffArgs;

/// Show the changes and conflicts between two branches.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_DIFF_BEGIN` | `lore_branch_diff_begin_event_data_t` | Emitted before diff results begin streaming |
/// | `LORE_EVENT_BRANCH_DIFF_CHANGE_BEGIN` | `lore_branch_diff_change_begin_event_data_t` | Emitted before the list of changed files begins |
/// | `LORE_EVENT_BRANCH_DIFF_CHANGE` | `lore_branch_diff_change_event_data_t` | Emitted for each changed file between the two branches |
/// | `LORE_EVENT_BRANCH_DIFF_CHANGE_END` | `lore_branch_diff_change_end_event_data_t` | Emitted after all changed files have been reported |
/// | `LORE_EVENT_BRANCH_DIFF_CONFLICT_BEGIN` | `lore_branch_diff_conflict_begin_event_data_t` | Emitted before the list of conflicting files begins |
/// | `LORE_EVENT_BRANCH_DIFF_CONFLICT` | `lore_branch_diff_conflict_event_data_t` | Emitted for each file that has a conflict between the two branches |
/// | `LORE_EVENT_BRANCH_DIFF_CONFLICT_END` | `lore_branch_diff_conflict_end_event_data_t` | Emitted after all conflict files have been reported |
/// | `LORE_EVENT_BRANCH_DIFF_END` | `lore_branch_diff_end_event_data_t` | Emitted after all diff results have been streamed |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_diff(
    globals: &LoreGlobalArgs,
    args: &LoreBranchDiffArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::diff)
}

/// Asynchronous version of `lore_branch_diff`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_DIFF_BEGIN` | `lore_branch_diff_begin_event_data_t` | Emitted before diff results begin streaming |
/// | `LORE_EVENT_BRANCH_DIFF_CHANGE_BEGIN` | `lore_branch_diff_change_begin_event_data_t` | Emitted before the list of changed files begins |
/// | `LORE_EVENT_BRANCH_DIFF_CHANGE` | `lore_branch_diff_change_event_data_t` | Emitted for each changed file between the two branches |
/// | `LORE_EVENT_BRANCH_DIFF_CHANGE_END` | `lore_branch_diff_change_end_event_data_t` | Emitted after all changed files have been reported |
/// | `LORE_EVENT_BRANCH_DIFF_CONFLICT_BEGIN` | `lore_branch_diff_conflict_begin_event_data_t` | Emitted before the list of conflicting files begins |
/// | `LORE_EVENT_BRANCH_DIFF_CONFLICT` | `lore_branch_diff_conflict_event_data_t` | Emitted for each file that has a conflict between the two branches |
/// | `LORE_EVENT_BRANCH_DIFF_CONFLICT_END` | `lore_branch_diff_conflict_end_event_data_t` | Emitted after all conflict files have been reported |
/// | `LORE_EVENT_BRANCH_DIFF_END` | `lore_branch_diff_end_event_data_t` | Emitted after all diff results have been streamed |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_diff_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchDiffArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::diff);
}

pub type LoreBranchProtectArgs = crate::branch::LoreBranchProtectArgs;

/// Enable write protection on a branch to prevent direct commits.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_PROTECT` | `lore_branch_protect_event_data_t` | Emitted when the branch has been successfully protected |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_protect(
    globals: &LoreGlobalArgs,
    args: &LoreBranchProtectArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::protect)
}

/// Asynchronous version of `lore_branch_protect`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_PROTECT` | `lore_branch_protect_event_data_t` | Emitted when the branch has been successfully protected |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_protect_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchProtectArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::protect);
}

pub type LoreBranchUnprotectArgs = crate::branch::LoreBranchUnprotectArgs;

/// Remove write protection from a branch.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_UNPROTECT` | `lore_branch_unprotect_event_data_t` | Emitted when the branch has been successfully unprotected |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_unprotect(
    globals: &LoreGlobalArgs,
    args: &LoreBranchUnprotectArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::unprotect)
}

/// Asynchronous version of `lore_branch_unprotect`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_UNPROTECT` | `lore_branch_unprotect_event_data_t` | Emitted when the branch has been successfully unprotected |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_unprotect_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchUnprotectArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::unprotect);
}

pub type LoreBranchArchiveArgs = crate::branch::LoreBranchArchiveArgs;

/// Archive a branch in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_ARCHIVE` | `lore_branch_archive_event_data_t` | Emitted when the branch has been successfully archived |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_archive(
    globals: &LoreGlobalArgs,
    args: &LoreBranchArchiveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::archive)
}

/// Asynchronous version of `lore_branch_archive`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_ARCHIVE` | `lore_branch_archive_event_data_t` | Emitted when the branch has been successfully archived |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_archive_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchArchiveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::archive);
}

pub type LoreBranchListArgs = crate::branch::LoreBranchListArgs;

/// List all branches in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_LIST_BEGIN` | `lore_branch_list_begin_event_data_t` | Emitted before branch list entries begin streaming |
/// | `LORE_EVENT_BRANCH_LIST_ENTRY` | `lore_branch_list_entry_event_data_t` | Emitted for each branch in the repository |
/// | `LORE_EVENT_BRANCH_LIST_END` | `lore_branch_list_end_event_data_t` | Emitted after all branch entries have been streamed |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_list(
    globals: &LoreGlobalArgs,
    args: &LoreBranchListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::list)
}

/// Asynchronous version of `lore_branch_list`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_LIST_BEGIN` | `lore_branch_list_begin_event_data_t` | Emitted before branch list entries begin streaming |
/// | `LORE_EVENT_BRANCH_LIST_ENTRY` | `lore_branch_list_entry_event_data_t` | Emitted for each branch in the repository |
/// | `LORE_EVENT_BRANCH_LIST_END` | `lore_branch_list_end_event_data_t` | Emitted after all branch entries have been streamed |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::list);
}

pub type LoreBranchMergeAbortArgs = crate::branch::LoreBranchMergeAbortArgs;

/// Abort an in-progress branch merge and restore the pre-merge state.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_ABORT_BEGIN` | `lore_branch_merge_abort_begin_event_data_t` | Emitted when aborting a branch merge, includes staged and current revision hashes |
/// | `LORE_EVENT_BRANCH_MERGE_ABORT_END` | `lore_branch_merge_abort_end_event_data_t` | Emitted after the merge abort has been completed |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization while reverting merge changes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_abort(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeAbortArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_abort)
}

/// Asynchronous version of `lore_branch_merge_abort`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_ABORT_BEGIN` | `lore_branch_merge_abort_begin_event_data_t` | Emitted when aborting a branch merge, includes staged and current revision hashes |
/// | `LORE_EVENT_BRANCH_MERGE_ABORT_END` | `lore_branch_merge_abort_end_event_data_t` | Emitted after the merge abort has been completed |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization while reverting merge changes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_abort_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeAbortArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_abort);
}

pub type LoreBranchMergeUnresolveArgs = crate::branch::LoreBranchMergeUnresolveArgs;

/// Mark conflicting files in a merge as unresolved.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_UNRESOLVE_FILE` | `lore_branch_merge_unresolve_file_event_data_t` | Emitted for each file that was marked as unresolved |
/// | `LORE_EVENT_BRANCH_MERGE_UNRESOLVE_REVISION` | `lore_branch_merge_unresolve_revision_event_data_t` | Emitted with the updated staged revision after unresolve completes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_unresolve(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeUnresolveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_unresolve)
}

/// Asynchronous version of `lore_branch_merge_unresolve`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_UNRESOLVE_FILE` | `lore_branch_merge_unresolve_file_event_data_t` | Emitted for each file that was marked as unresolved |
/// | `LORE_EVENT_BRANCH_MERGE_UNRESOLVE_REVISION` | `lore_branch_merge_unresolve_revision_event_data_t` | Emitted with the updated staged revision after unresolve completes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_unresolve_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeUnresolveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_unresolve);
}

pub type LoreBranchMergeIntoArgs = crate::branch::LoreBranchMergeIntoArgs;

/// Merge the current branch into a target branch.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FILE_BEGIN` | `lore_branch_merge_into_file_begin_event_data_t` | Emitted when starting to merge files into the target branch |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FILE` | `lore_branch_merge_into_file_event_data_t` | Emitted for each file being merged into the target branch |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FILE_END` | `lore_branch_merge_into_file_end_event_data_t` | Emitted after all files have been merged |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_BEGIN` | `lore_branch_merge_into_fragment_begin_event_data_t` | Emitted when starting fragment transfer for a file |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_PROGRESS` | `lore_branch_merge_into_fragment_progress_event_data_t` | Emitted periodically during fragment transfer |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_END` | `lore_branch_merge_into_fragment_end_event_data_t` | Emitted when fragment transfer for a file completes |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_REVISION` | `lore_branch_merge_into_revision_event_data_t` | Emitted with the resulting revision after the merge into is complete |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_SYNC_BEGIN` | `lore_branch_merge_into_sync_begin_event_data_t` | Emitted when starting to apply the changes on the target state |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_SYNC_END` | `lore_branch_merge_into_sync_end_event_data_t` | Emitted after applying the changes on the target state is complete |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit starts (if no conflicts) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted periodically during auto-commit file processing |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit file processing completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revision details |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during changes realization |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the committed revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each file fragment written or deduplicated during commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_into(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeIntoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_into)
}

/// Asynchronous version of `lore_branch_merge_into`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FILE_BEGIN` | `lore_branch_merge_into_file_begin_event_data_t` | Emitted when starting to merge files into the target branch |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FILE` | `lore_branch_merge_into_file_event_data_t` | Emitted for each file being merged into the target branch |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FILE_END` | `lore_branch_merge_into_file_end_event_data_t` | Emitted after all files have been merged |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_BEGIN` | `lore_branch_merge_into_fragment_begin_event_data_t` | Emitted when starting fragment transfer for a file |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_PROGRESS` | `lore_branch_merge_into_fragment_progress_event_data_t` | Emitted periodically during fragment transfer |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_END` | `lore_branch_merge_into_fragment_end_event_data_t` | Emitted when fragment transfer for a file completes |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_REVISION` | `lore_branch_merge_into_revision_event_data_t` | Emitted with the resulting revision after the merge into is complete |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_SYNC_BEGIN` | `lore_branch_merge_into_sync_begin_event_data_t` | Emitted when starting to apply the changes on the target state |
/// | `LORE_EVENT_BRANCH_MERGE_INTO_SYNC_END` | `lore_branch_merge_into_sync_end_event_data_t` | Emitted after applying the changes on the target state is complete |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit starts (if no conflicts) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted periodically during auto-commit file processing |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit file processing completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revision details |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during changes realization |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the committed revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each file fragment written or deduplicated during commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_into_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeIntoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_into);
}

pub type LoreBranchMergeResolveArgs = crate::branch::LoreBranchMergeResolveArgs;

/// Mark conflicting files in a merge as resolved.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE` | `lore_branch_merge_resolve_file_event_data_t` | Emitted for each file that was marked as resolved |
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION` | `lore_branch_merge_resolve_revision_event_data_t` | Emitted with the updated staged revision after resolve completes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_resolve(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeResolveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_resolve)
}

/// Asynchronous version of `lore_branch_merge_resolve`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE` | `lore_branch_merge_resolve_file_event_data_t` | Emitted for each file that was marked as resolved |
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION` | `lore_branch_merge_resolve_revision_event_data_t` | Emitted with the updated staged revision after resolve completes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_resolve_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeResolveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_resolve);
}

pub type LoreBranchMergeResolveMineArgs = crate::branch::LoreBranchMergeResolveMineArgs;

/// Resolve a merge conflict by accepting the "mine" version of each conflicting file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE` | `lore_branch_merge_resolve_file_event_data_t` | Emitted for each file resolved by keeping "mine" |
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION` | `lore_branch_merge_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_resolve_mine(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeResolveMineArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_resolve_mine)
}

/// Asynchronous version of `lore_branch_merge_resolve_mine`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE` | `lore_branch_merge_resolve_file_event_data_t` | Emitted for each file resolved by keeping "mine" |
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION` | `lore_branch_merge_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_resolve_mine_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeResolveMineArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_resolve_mine);
}

pub type LoreBranchMergeResolveTheirsArgs = crate::branch::LoreBranchMergeResolveTheirsArgs;

/// Resolve a merge conflict by accepting the "theirs" version of each conflicting file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE` | `lore_branch_merge_resolve_file_event_data_t` | Emitted for each file resolved by keeping "theirs" |
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION` | `lore_branch_merge_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_resolve_theirs(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeResolveTheirsArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_resolve_theirs)
}

/// Asynchronous version of `lore_branch_merge_resolve_theirs`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE` | `lore_branch_merge_resolve_file_event_data_t` | Emitted for each file resolved by keeping "theirs" |
/// | `LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION` | `lore_branch_merge_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_resolve_theirs_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeResolveTheirsArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_resolve_theirs);
}

pub type LoreBranchMergeRestartArgs = crate::branch::LoreBranchMergeRestartArgs;

/// Restart an in-progress merge, re-materializing conflicted files.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE` | `lore_branch_merge_conflict_file_event_data_t` | Emitted for each file with a remaining merge conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization during restart |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file re-materialized during restart |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_restart(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeRestartArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_restart)
}

/// Asynchronous version of `lore_branch_merge_restart`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE` | `lore_branch_merge_conflict_file_event_data_t` | Emitted for each file with a remaining merge conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization during restart |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file re-materialized during restart |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_restart_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeRestartArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_restart);
}

pub type LoreBranchMergeStartArgs = crate::branch::LoreBranchMergeStartArgs;

/// Start a merge from another branch into the current branch.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_START_BEGIN` | `lore_branch_merge_start_begin_event_data_t` | Emitted when merge begins, includes source branch and revision info |
/// | `LORE_EVENT_BRANCH_MERGE_START_END` | `lore_branch_merge_start_end_event_data_t` | Emitted when merge operation completes, includes sync stats and conflict flag |
/// | `LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE` | `lore_branch_merge_conflict_file_event_data_t` | Emitted for each file with an unresolved merge conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during the apply_diff phase of the merge |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file modified during merge realization |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged for deletion during merge realization |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit starts (no conflicts, no_commit=false) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted periodically during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit file processing completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revision details |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the committed revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written during auto-commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_start(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeStartArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::merge_start)
}

/// Asynchronous version of `lore_branch_merge_start`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_MERGE_START_BEGIN` | `lore_branch_merge_start_begin_event_data_t` | Emitted when merge begins, includes source branch and revision info |
/// | `LORE_EVENT_BRANCH_MERGE_START_END` | `lore_branch_merge_start_end_event_data_t` | Emitted when merge operation completes, includes sync stats and conflict flag |
/// | `LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE` | `lore_branch_merge_conflict_file_event_data_t` | Emitted for each file with an unresolved merge conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during the apply_diff phase of the merge |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file modified during merge realization |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged for deletion during merge realization |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit starts (no conflicts, no_commit=false) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted periodically during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit file processing completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revision details |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the committed revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written during auto-commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_merge_start_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMergeStartArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::merge_start);
}

pub type LoreBranchSwitchArgs = crate::branch::LoreBranchSwitchArgs;

/// Switch to a different branch and update the working directory.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_SWITCH_BEGIN` | `lore_branch_switch_begin_event_data_t` | Emitted when branch switch starts |
/// | `LORE_EVENT_BRANCH_SWITCH_END` | `lore_branch_switch_end_event_data_t` | Emitted when branch switch completes successfully |
/// | `LORE_EVENT_REVISION_SYNC_TARGET` | `lore_revision_sync_target_event_data_t` | Emitted with target revision info after resolving the switch target |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file modified/added/deleted during switch |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted periodically during file realization |
/// | `LORE_EVENT_REVISION_SYNC_REVISION` | `lore_revision_sync_revision_event_data_t` | Emitted with the resulting revision after switch |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view or ignore filters |
/// | `LORE_EVENT_REVISION_RESOLVE` | `lore_revision_resolve_event_data_t` | Emitted when resolving a partial revision reference |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_switch(
    globals: &LoreGlobalArgs,
    args: &LoreBranchSwitchArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::switch)
}

/// Asynchronous version of `lore_branch_switch`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_SWITCH_BEGIN` | `lore_branch_switch_begin_event_data_t` | Emitted when branch switch starts |
/// | `LORE_EVENT_BRANCH_SWITCH_END` | `lore_branch_switch_end_event_data_t` | Emitted when branch switch completes successfully |
/// | `LORE_EVENT_REVISION_SYNC_TARGET` | `lore_revision_sync_target_event_data_t` | Emitted with target revision info after resolving the switch target |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file modified/added/deleted during switch |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted periodically during file realization |
/// | `LORE_EVENT_REVISION_SYNC_REVISION` | `lore_revision_sync_revision_event_data_t` | Emitted with the resulting revision after switch |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view or ignore filters |
/// | `LORE_EVENT_REVISION_RESOLVE` | `lore_revision_resolve_event_data_t` | Emitted when resolving a partial revision reference |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_switch_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchSwitchArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::switch);
}

pub type LoreBranchResetArgs = crate::branch::LoreBranchResetArgs;

/// Reset the current branch to a specific revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_RESET` | `lore_branch_reset_event_data_t` | Emitted when the branch has been reset to the target revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_reset(
    globals: &LoreGlobalArgs,
    args: &LoreBranchResetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::reset)
}

/// Asynchronous version of `lore_branch_reset`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_RESET` | `lore_branch_reset_event_data_t` | Emitted when the branch has been reset to the target revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_reset_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchResetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::reset);
}

pub type LoreBranchPushArgs = crate::branch::LoreBranchPushArgs;

/// Push local branch commits to the remote repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_PUSH` | `lore_branch_push_event_data_t` | Emitted when push begins, includes branch name and revision info |
/// | `LORE_EVENT_BRANCH_PUSH_BRANCH_CREATE_BEGIN` | `lore_branch_push_branch_create_begin_event_data_t` | Emitted when creating the remote branch (first push) |
/// | `LORE_EVENT_BRANCH_PUSH_BRANCH_CREATE_END` | `lore_branch_push_branch_create_end_event_data_t` | Emitted when remote branch creation completes |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_UPDATE_BEGIN` | `lore_branch_push_revision_update_begin_event_data_t` | Emitted when updating a revision on the remote |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_UPDATE_END` | `lore_branch_push_revision_update_end_event_data_t` | Emitted when a revision update completes |
/// | `LORE_EVENT_BRANCH_PUSH_FRAGMENT_BEGIN` | `lore_branch_push_fragment_begin_event_data_t` | Emitted when uploading fragment data begins |
/// | `LORE_EVENT_BRANCH_PUSH_FRAGMENT_PROGRESS` | `lore_branch_push_fragment_progress_event_data_t` | Emitted periodically during fragment upload |
/// | `LORE_EVENT_BRANCH_PUSH_FRAGMENT_END` | `lore_branch_push_fragment_end_event_data_t` | Emitted when fragment upload completes |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_BEGIN` | `lore_branch_push_revision_push_begin_event_data_t` | Emitted when pushing a revision to the remote begins |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_UPDATE` | `lore_branch_push_revision_push_update_event_data_t` | Emitted with progress updates during revision push |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_END` | `lore_branch_push_revision_push_end_event_data_t` | Emitted when revision push completes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_push(
    globals: &LoreGlobalArgs,
    args: &LoreBranchPushArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::push)
}

/// Asynchronous version of `lore_branch_push`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Branch Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_BRANCH_PUSH` | `lore_branch_push_event_data_t` | Emitted when push begins, includes branch name and revision info |
/// | `LORE_EVENT_BRANCH_PUSH_BRANCH_CREATE_BEGIN` | `lore_branch_push_branch_create_begin_event_data_t` | Emitted when creating the remote branch (first push) |
/// | `LORE_EVENT_BRANCH_PUSH_BRANCH_CREATE_END` | `lore_branch_push_branch_create_end_event_data_t` | Emitted when remote branch creation completes |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_UPDATE_BEGIN` | `lore_branch_push_revision_update_begin_event_data_t` | Emitted when updating a revision on the remote |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_UPDATE_END` | `lore_branch_push_revision_update_end_event_data_t` | Emitted when a revision update completes |
/// | `LORE_EVENT_BRANCH_PUSH_FRAGMENT_BEGIN` | `lore_branch_push_fragment_begin_event_data_t` | Emitted when uploading fragment data begins |
/// | `LORE_EVENT_BRANCH_PUSH_FRAGMENT_PROGRESS` | `lore_branch_push_fragment_progress_event_data_t` | Emitted periodically during fragment upload |
/// | `LORE_EVENT_BRANCH_PUSH_FRAGMENT_END` | `lore_branch_push_fragment_end_event_data_t` | Emitted when fragment upload completes |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_BEGIN` | `lore_branch_push_revision_push_begin_event_data_t` | Emitted when pushing a revision to the remote begins |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_UPDATE` | `lore_branch_push_revision_push_update_event_data_t` | Emitted with progress updates during revision push |
/// | `LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_END` | `lore_branch_push_revision_push_end_event_data_t` | Emitted when revision push completes |
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_push_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchPushArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::push);
}

pub type LoreBranchMetadataGetArgs = crate::branch::LoreBranchMetadataGetArgs;

/// Retrieve branch metadata.
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_metadata_get(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::metadata_get)
}

/// Asynchronous version of `lore_branch_metadata_get`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_metadata_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::metadata_get);
}

pub type LoreBranchMetadataSetArgs = crate::branch::LoreBranchMetadataSetArgs;

/// Set branch metadata key-value pairs.
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_metadata_set(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::metadata_set)
}

/// Asynchronous version of `lore_branch_metadata_set`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_metadata_set_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::metadata_set);
}

pub type LoreBranchMetadataClearArgs = crate::branch::LoreBranchMetadataClearArgs;

/// Clear branch metadata keys.
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_metadata_clear(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::branch::metadata_clear)
}

/// Asynchronous version of `lore_branch_metadata_clear`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_branch_metadata_clear_async(
    globals: &LoreGlobalArgs,
    args: &LoreBranchMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::branch::metadata_clear);
}

pub type LoreFileInfoArgs = crate::file::LoreFileInfoArgs;

/// Retrieve metadata for one or more files in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_INFO` | `lore_file_info_event_data_t` | Emitted for each file with its metadata (size, hash, staged status, etc.) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_info(
    globals: &LoreGlobalArgs,
    args: &LoreFileInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::info)
}

/// Asynchronous version of `lore_file_info`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_INFO` | `lore_file_info_event_data_t` | Emitted for each file with its metadata (size, hash, staged status, etc.) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::info);
}

pub type LoreFileDiffArgs = crate::file::LoreFileDiffArgs;

/// Show which files differ between two revisions.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DIFF` | `lore_file_diff_event_data_t` | Emitted for each file that differs between the two revisions |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_diff(
    globals: &LoreGlobalArgs,
    args: &LoreFileDiffArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::diff)
}

/// Asynchronous version of `lore_file_diff`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DIFF` | `lore_file_diff_event_data_t` | Emitted for each file that differs between the two revisions |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_diff_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDiffArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::diff);
}

pub type LoreFileHashArgs = crate::file::LoreFileHashArgs;

/// Compute the hash of a local file for comparison with repository content.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_HASH` | `lore_file_hash_event_data_t` | Emitted with the computed hash and size of the specified file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_hash(
    globals: &LoreGlobalArgs,
    args: &LoreFileHashArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::hash)
}

/// Asynchronous version of `lore_file_hash`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_HASH` | `lore_file_hash_event_data_t` | Emitted with the computed hash and size of the specified file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_hash_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileHashArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::hash);
}

pub type LoreFileHistoryArgs = crate::file::LoreFileHistoryArgs;

/// Retrieve the revision history for a specific file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_HISTORY` | `lore_file_history_event_data_t` | Emitted for each revision in which the file was modified |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_history(
    globals: &LoreGlobalArgs,
    args: &LoreFileHistoryArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::history)
}

/// Asynchronous version of `lore_file_history`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_HISTORY` | `lore_file_history_event_data_t` | Emitted for each revision in which the file was modified |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_history_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileHistoryArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::history);
}

pub type LoreFileMetadataClearArgs = crate::file::LoreFileMetadataClearArgs;

/// Clear all metadata from a file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA_CLEAR_FILE` | `lore_metadata_clear_file_event_data_t` | Emitted when metadata has been cleared for the file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_clear(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::metadata_clear)
}

/// Asynchronous version of `lore_file_metadata_clear`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA_CLEAR_FILE` | `lore_metadata_clear_file_event_data_t` | Emitted when metadata has been cleared for the file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_clear_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::metadata_clear);
}

pub type LoreFileMetadataGetArgs = crate::file::LoreFileMetadataGetArgs;

/// Get a specific metadata key/value pair from a file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for the requested metadata key/value pair |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_get(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::metadata_get)
}

/// Asynchronous version of `lore_file_metadata_get`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for the requested metadata key/value pair |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::metadata_get);
}

pub type LoreFileMetadataListArgs = crate::file::LoreFileMetadataListArgs;

/// List all metadata key/value pairs associated with a file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata key/value pair associated with the file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_list(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::metadata_list)
}

/// Asynchronous version of `lore_file_metadata_list`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata key/value pair associated with the file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::metadata_list);
}

pub type LoreFileMetadataSetArgs = crate::file::LoreFileMetadataSetArgs;

/// Set a metadata key/value pair on a file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_set(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::metadata_set)
}

/// Asynchronous version of `lore_file_metadata_set`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_metadata_set_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::metadata_set);
}

pub type LoreFileResetArgs = crate::file::LoreFileResetArgs;

/// Reset files to the state recorded in the current or target revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_RESET_BEGIN` | `lore_file_reset_begin_event_data_t` | Emitted when reset starts, includes path count |
/// | `LORE_EVENT_FILE_RESET_PROGRESS` | `lore_file_reset_progress_event_data_t` | Emitted periodically during file reset with progress counts |
/// | `LORE_EVENT_FILE_RESET_END` | `lore_file_reset_end_event_data_t` | Emitted when reset completes |
/// | `LORE_EVENT_FILE_RESET_FILE` | `lore_file_reset_file_event_data_t` | Emitted for each file that was reset |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file materialized |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by filters |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_reset(
    globals: &LoreGlobalArgs,
    args: &LoreFileResetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::reset)
}

/// Asynchronous version of `lore_file_reset`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_RESET_BEGIN` | `lore_file_reset_begin_event_data_t` | Emitted when reset starts, includes path count |
/// | `LORE_EVENT_FILE_RESET_PROGRESS` | `lore_file_reset_progress_event_data_t` | Emitted periodically during file reset with progress counts |
/// | `LORE_EVENT_FILE_RESET_END` | `lore_file_reset_end_event_data_t` | Emitted when reset completes |
/// | `LORE_EVENT_FILE_RESET_FILE` | `lore_file_reset_file_event_data_t` | Emitted for each file that was reset |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file materialized |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by filters |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_reset_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileResetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::reset);
}

pub type LoreFileResetToLastMergedArgs = crate::file::LoreFileResetToLastMergedArgs;

/// Reset files to their state at the last merged revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_RESET_BEGIN` | `lore_file_reset_begin_event_data_t` | Emitted when reset starts |
/// | `LORE_EVENT_FILE_RESET_PROGRESS` | `lore_file_reset_progress_event_data_t` | Emitted periodically during file reset |
/// | `LORE_EVENT_FILE_RESET_END` | `lore_file_reset_end_event_data_t` | Emitted when reset completes |
/// | `LORE_EVENT_FILE_RESET_FILE` | `lore_file_reset_file_event_data_t` | Emitted for each file that was reset |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file materialized |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_reset_to_last_merged(
    globals: &LoreGlobalArgs,
    args: &LoreFileResetToLastMergedArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::reset_to_last_merged)
}

/// Asynchronous version of `lore_file_reset_to_last_merged`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_RESET_BEGIN` | `lore_file_reset_begin_event_data_t` | Emitted when reset starts |
/// | `LORE_EVENT_FILE_RESET_PROGRESS` | `lore_file_reset_progress_event_data_t` | Emitted periodically during file reset |
/// | `LORE_EVENT_FILE_RESET_END` | `lore_file_reset_end_event_data_t` | Emitted when reset completes |
/// | `LORE_EVENT_FILE_RESET_FILE` | `lore_file_reset_file_event_data_t` | Emitted for each file that was reset |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file materialized |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_reset_to_last_merged_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileResetToLastMergedArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::reset_to_last_merged);
}

pub type LoreFileStageArgs = crate::file::LoreFileStageArgs;

/// Stage files for the next commit.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_STAGE_BEGIN` | `lore_file_stage_begin_event_data_t` | Emitted when staging begins, includes path count |
/// | `LORE_EVENT_FILE_STAGE_PROGRESS` | `lore_file_stage_progress_event_data_t` | Emitted periodically during staging with file counts |
/// | `LORE_EVENT_FILE_STAGE_END` | `lore_file_stage_end_event_data_t` | Emitted when staging completes |
/// | `LORE_EVENT_FILE_STAGE_REVISION` | `lore_file_stage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged or staged for deletion |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by filters |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_stage(
    globals: &LoreGlobalArgs,
    args: &LoreFileStageArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::stage)
}

/// Asynchronous version of `lore_file_stage`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_STAGE_BEGIN` | `lore_file_stage_begin_event_data_t` | Emitted when staging begins, includes path count |
/// | `LORE_EVENT_FILE_STAGE_PROGRESS` | `lore_file_stage_progress_event_data_t` | Emitted periodically during staging with file counts |
/// | `LORE_EVENT_FILE_STAGE_END` | `lore_file_stage_end_event_data_t` | Emitted when staging completes |
/// | `LORE_EVENT_FILE_STAGE_REVISION` | `lore_file_stage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged or staged for deletion |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by filters |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_stage_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileStageArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::stage);
}

pub type LoreFileStageMergeArgs = crate::file::LoreFileStageMergeArgs;

/// Stage files for a merge commit, recording resolved merge content.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_STAGE_BEGIN` | `lore_file_stage_begin_event_data_t` | Emitted when merge-staging begins |
/// | `LORE_EVENT_FILE_STAGE_PROGRESS` | `lore_file_stage_progress_event_data_t` | Emitted periodically during merge-staging |
/// | `LORE_EVENT_FILE_STAGE_REVISION` | `lore_file_stage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_stage_merge(
    globals: &LoreGlobalArgs,
    args: &LoreFileStageMergeArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::stage_merge)
}

/// Asynchronous version of `lore_file_stage_merge`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_STAGE_BEGIN` | `lore_file_stage_begin_event_data_t` | Emitted when merge-staging begins |
/// | `LORE_EVENT_FILE_STAGE_PROGRESS` | `lore_file_stage_progress_event_data_t` | Emitted periodically during merge-staging |
/// | `LORE_EVENT_FILE_STAGE_REVISION` | `lore_file_stage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_stage_merge_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileStageMergeArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::stage_merge);
}

pub type LoreFileStageMoveArgs = crate::file::LoreFileStageMoveArgs;

/// Stage a file move (rename) operation for commit.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_STAGE_BEGIN` | `lore_file_stage_begin_event_data_t` | Emitted when move staging begins |
/// | `LORE_EVENT_FILE_STAGE_END` | `lore_file_stage_end_event_data_t` | Emitted when move staging completes |
/// | `LORE_EVENT_FILE_STAGE_REVISION` | `lore_file_stage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged (deletion of original and new path) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_stage_move(
    globals: &LoreGlobalArgs,
    args: &LoreFileStageMoveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::stage_move)
}

/// Asynchronous version of `lore_file_stage_move`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_STAGE_BEGIN` | `lore_file_stage_begin_event_data_t` | Emitted when move staging begins |
/// | `LORE_EVENT_FILE_STAGE_END` | `lore_file_stage_end_event_data_t` | Emitted when move staging completes |
/// | `LORE_EVENT_FILE_STAGE_REVISION` | `lore_file_stage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged (deletion of original and new path) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_stage_move_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileStageMoveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::stage_move);
}

pub type LoreFileDirtyArgs = crate::file::LoreFileDirtyArgs;

/// Mark files as dirty in the staged state without staging their content.
///
/// Action is determined by checking filesystem existence and current revision state
/// (modify, add, delete, or revert-add). Respects ignore and view filters.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_PATH_IGNORE` | `lore_path_ignore_event_data_t` | Emitted for each input path that could not be resolved to a repository-relative path |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view or ignore filters |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dirty(
    globals: &LoreGlobalArgs,
    args: &LoreFileDirtyArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::dirty)
}

/// Asynchronous version of `lore_file_dirty`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_PATH_IGNORE` | `lore_path_ignore_event_data_t` | Emitted for each input path that could not be resolved to a repository-relative path |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view or ignore filters |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dirty_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDirtyArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::dirty);
}

pub type LoreFileDirtyMoveArgs = crate::file::LoreFileDirtyMoveArgs;

/// Mark a file as dirty-moved from one path to another in the staged state.
///
/// Updates the source node's parent/name and flags it with `DirtyMove`, propagating
/// `Dirty` to both the old and new parent directories. For directories, the move
/// is propagated recursively to children. No filesystem access is performed.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dirty_move(
    globals: &LoreGlobalArgs,
    args: &LoreFileDirtyMoveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::dirty_move)
}

/// Asynchronous version of `lore_file_dirty_move`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dirty_move_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDirtyMoveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::dirty_move);
}

pub type LoreFileDirtyCopyArgs = crate::file::LoreFileDirtyCopyArgs;

/// Mark a file as dirty-copied from one path to another in the staged state.
///
/// Creates a new destination node flagged `DirtyCopy`; the source node is unchanged.
/// No filesystem access is performed.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dirty_copy(
    globals: &LoreGlobalArgs,
    args: &LoreFileDirtyCopyArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::dirty_copy)
}

/// Asynchronous version of `lore_file_dirty_copy`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dirty_copy_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDirtyCopyArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::dirty_copy);
}

pub type LoreFileUnstageArgs = crate::file::LoreFileUnstageArgs;

/// Remove files from the staging area without discarding local changes.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_UNSTAGE_BEGIN` | `lore_file_unstage_begin_event_data_t` | Emitted when unstage begins, includes path count |
/// | `LORE_EVENT_FILE_UNSTAGE_PROGRESS` | `lore_file_unstage_progress_event_data_t` | Emitted periodically during unstaging |
/// | `LORE_EVENT_FILE_UNSTAGE_END` | `lore_file_unstage_end_event_data_t` | Emitted when unstaging completes |
/// | `LORE_EVENT_FILE_UNSTAGE_REVISION` | `lore_file_unstage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_UNSTAGE_FILE` | `lore_file_unstage_file_event_data_t` | Emitted for each file that was unstaged |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_unstage(
    globals: &LoreGlobalArgs,
    args: &LoreFileUnstageArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::unstage)
}

/// Asynchronous version of `lore_file_unstage`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_UNSTAGE_BEGIN` | `lore_file_unstage_begin_event_data_t` | Emitted when unstage begins, includes path count |
/// | `LORE_EVENT_FILE_UNSTAGE_PROGRESS` | `lore_file_unstage_progress_event_data_t` | Emitted periodically during unstaging |
/// | `LORE_EVENT_FILE_UNSTAGE_END` | `lore_file_unstage_end_event_data_t` | Emitted when unstaging completes |
/// | `LORE_EVENT_FILE_UNSTAGE_REVISION` | `lore_file_unstage_revision_event_data_t` | Emitted with the resulting staged revision |
/// | `LORE_EVENT_FILE_UNSTAGE_FILE` | `lore_file_unstage_file_event_data_t` | Emitted for each file that was unstaged |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_unstage_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileUnstageArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::unstage);
}

pub type LoreFileWriteArgs = crate::file::LoreFileWriteArgs;

/// Write binary content to a file in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_WRITE` | `lore_file_write_event_data_t` | Emitted when the file has been successfully written to the repository |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_write(
    globals: &LoreGlobalArgs,
    args: &LoreFileWriteArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::write)
}

/// Asynchronous version of `lore_file_write`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_WRITE` | `lore_file_write_event_data_t` | Emitted when the file has been successfully written to the repository |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_write_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileWriteArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::write);
}

pub type LoreFileObliterateArgs = crate::file::LoreFileObliterateArgs;

/// Permanently remove a file and all its history from the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_OBLITERATE` | `lore_file_obliterate_event_data_t` | Emitted for each file permanently removed from repository history |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_obliterate(
    globals: &LoreGlobalArgs,
    args: &LoreFileObliterateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::obliterate)
}

/// Asynchronous version of `lore_file_obliterate`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_OBLITERATE` | `lore_file_obliterate_event_data_t` | Emitted for each file permanently removed from repository history |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_obliterate_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileObliterateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::obliterate);
}

pub type LoreFileDumpArgs = crate::file::LoreFileDumpArgs;

/// Retrieve the binary content of a file at a specific revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DUMP` | `lore_file_dump_event_data_t` | Emitted with binary content of the requested file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dump(
    globals: &LoreGlobalArgs,
    args: &LoreFileDumpArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::file::dump)
}

/// Asynchronous version of `lore_file_dump`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## File Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DUMP` | `lore_file_dump_event_data_t` | Emitted with binary content of the requested file |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dump_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDumpArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::file::dump);
}

pub type LoreFileDependencyAddArgs = crate::dependency::LoreFileDependencyAddArgs;

/// Adds dependency relationships between files.
///
/// # Events
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Dependency Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DEPENDENCY_ADD_BEGIN` | `lore_file_dependency_add_begin_event_data_t` | Start of operation |
/// | `LORE_EVENT_FILE_DEPENDENCY_ADD_ENTRY` | `lore_file_dependency_add_entry_event_data_t` | Each dependency added |
/// | `LORE_EVENT_FILE_DEPENDENCY_ADD_END` | `lore_file_dependency_add_end_event_data_t` | Operation complete |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dependency_add(
    globals: &LoreGlobalArgs,
    args: &LoreFileDependencyAddArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::dependency::dependency_add)
}

/// Asynchronous version of `lore_file_dependency_add`.
///
/// # Events
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Dependency Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DEPENDENCY_ADD_BEGIN` | `lore_file_dependency_add_begin_event_data_t` | Start of operation |
/// | `LORE_EVENT_FILE_DEPENDENCY_ADD_ENTRY` | `lore_file_dependency_add_entry_event_data_t` | Each dependency added |
/// | `LORE_EVENT_FILE_DEPENDENCY_ADD_END` | `lore_file_dependency_add_end_event_data_t` | Operation complete |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dependency_add_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDependencyAddArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::dependency::dependency_add);
}

pub type LoreFileDependencyRemoveArgs = crate::dependency::LoreFileDependencyRemoveArgs;

/// Removes dependency relationships between files.
///
/// # Events
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Dependency Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DEPENDENCY_REMOVE_BEGIN` | `lore_file_dependency_remove_begin_event_data_t` | Start of operation |
/// | `LORE_EVENT_FILE_DEPENDENCY_REMOVE_ENTRY` | `lore_file_dependency_remove_entry_event_data_t` | Each dependency removed |
/// | `LORE_EVENT_FILE_DEPENDENCY_REMOVE_END` | `lore_file_dependency_remove_end_event_data_t` | Operation complete |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dependency_remove(
    globals: &LoreGlobalArgs,
    args: &LoreFileDependencyRemoveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::dependency::dependency_remove,
    )
}

/// Asynchronous version of `lore_file_dependency_remove`.
///
/// # Events
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Dependency Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DEPENDENCY_REMOVE_BEGIN` | `lore_file_dependency_remove_begin_event_data_t` | Start of operation |
/// | `LORE_EVENT_FILE_DEPENDENCY_REMOVE_ENTRY` | `lore_file_dependency_remove_entry_event_data_t` | Each dependency removed |
/// | `LORE_EVENT_FILE_DEPENDENCY_REMOVE_END` | `lore_file_dependency_remove_end_event_data_t` | Operation complete |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dependency_remove_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDependencyRemoveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::dependency::dependency_remove,
    );
}

pub type LoreFileDependencyListArgs = crate::dependency::LoreFileDependencyListArgs;

/// Queries dependency information for files.
///
/// # Events
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Dependency Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_BEGIN` | `lore_file_dependency_list_begin_event_data_t` | Start of listing |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_FILE` | `lore_file_dependency_list_file_event_data_t` | Start of entries for one file |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_ENTRY` | `lore_file_dependency_list_entry_event_data_t` | One dependency entry |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_FILE_END` | `lore_file_dependency_list_file_end_event_data_t` | End of entries for one file |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_END` | `lore_file_dependency_list_end_event_data_t` | End of listing |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dependency_list(
    globals: &LoreGlobalArgs,
    args: &LoreFileDependencyListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::dependency::dependency_list)
}

/// Asynchronous version of `lore_file_dependency_list`.
///
/// # Events
///
/// ## Standard Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Dependency Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_BEGIN` | `lore_file_dependency_list_begin_event_data_t` | Start of listing |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_FILE` | `lore_file_dependency_list_file_event_data_t` | Start of entries for one file |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_ENTRY` | `lore_file_dependency_list_entry_event_data_t` | One dependency entry |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_FILE_END` | `lore_file_dependency_list_file_end_event_data_t` | End of entries for one file |
/// | `LORE_EVENT_FILE_DEPENDENCY_LIST_END` | `lore_file_dependency_list_end_event_data_t` | End of listing |
#[unsafe(no_mangle)]
pub extern "C" fn lore_file_dependency_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreFileDependencyListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::dependency::dependency_list);
}

pub type LoreLockFileAcquireArgs = crate::lock::LoreLockFileAcquireArgs;

/// Acquire exclusive locks on one or more files in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_ACQUIRE` | `lore_lock_file_acquire_event_data_t` | Emitted for each file for which a lock was successfully acquired |
/// | `LORE_EVENT_LOCK_FILE_ACQUIRE_IGNORE` | `lore_lock_file_acquire_ignore_event_data_t` | Emitted for each file for which a lock was ignored (already owned) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_acquire(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileAcquireArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::lock::file_acquire)
}

/// Asynchronous version of `lore_lock_file_acquire`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_ACQUIRE` | `lore_lock_file_acquire_event_data_t` | Emitted for each file for which a lock was successfully acquired |
/// | `LORE_EVENT_LOCK_FILE_ACQUIRE_IGNORE` | `lore_lock_file_acquire_ignore_event_data_t` | Emitted for each file for which a lock was ignored (already owned) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_acquire_async(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileAcquireArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::lock::file_acquire);
}

pub type LoreLockFileStatusArgs = crate::lock::LoreLockFileStatusArgs;

/// Get the lock status of files in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_STATUS_BEGIN` | `lore_lock_file_status_begin_event_data_t` | Emitted before lock status results begin streaming |
/// | `LORE_EVENT_LOCK_FILE_STATUS` | `lore_lock_file_status_event_data_t` | Emitted for each locked file with owner and lock details |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_status(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileStatusArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::lock::file_status)
}

/// Asynchronous version of `lore_lock_file_status`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_STATUS_BEGIN` | `lore_lock_file_status_begin_event_data_t` | Emitted before lock status results begin streaming |
/// | `LORE_EVENT_LOCK_FILE_STATUS` | `lore_lock_file_status_event_data_t` | Emitted for each locked file with owner and lock details |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_status_async(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileStatusArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::lock::file_status);
}

pub type LoreLockFileQueryArgs = crate::lock::LoreLockFileQueryArgs;

/// Query which files are currently locked, optionally filtered by user or path.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_QUERY_BEGIN` | `lore_lock_file_query_begin_event_data_t` | Emitted before query results begin streaming |
/// | `LORE_EVENT_LOCK_FILE_QUERY` | `lore_lock_file_query_event_data_t` | Emitted for each file matching the query |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_query(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileQueryArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::lock::file_query)
}

/// Asynchronous version of `lore_lock_file_query`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_QUERY_BEGIN` | `lore_lock_file_query_begin_event_data_t` | Emitted before query results begin streaming |
/// | `LORE_EVENT_LOCK_FILE_QUERY` | `lore_lock_file_query_event_data_t` | Emitted for each file matching the query |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_query_async(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileQueryArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::lock::file_query);
}

pub type LoreLockFileReleaseArgs = crate::lock::LoreLockFileReleaseArgs;

/// Release file locks previously acquired by this client.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_RELEASE` | `lore_lock_file_release_event_data_t` | Emitted for each file lock successfully released |
/// | `LORE_EVENT_LOCK_FILE_RELEASE_NOT_FOUND` | `lore_lock_file_release_not_found_event_data_t` | Emitted for each file whose lock was not found |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_release(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileReleaseArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::lock::file_release)
}

/// Asynchronous version of `lore_lock_file_release`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Lock Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOCK_FILE_RELEASE` | `lore_lock_file_release_event_data_t` | Emitted for each file lock successfully released |
/// | `LORE_EVENT_LOCK_FILE_RELEASE_NOT_FOUND` | `lore_lock_file_release_not_found_event_data_t` | Emitted for each file whose lock was not found |
#[unsafe(no_mangle)]
pub extern "C" fn lore_lock_file_release_async(
    globals: &LoreGlobalArgs,
    args: &LoreLockFileReleaseArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::lock::file_release);
}

pub type LoreLinkAddArgs = crate::link::LoreLinkAddArgs;

/// Add a link to another repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_CLONE_BEGIN` | `lore_repository_clone_begin_event_data_t` | Emitted when cloning a linked repository begins |
/// | `LORE_EVENT_REPOSITORY_CLONE_END` | `lore_repository_clone_end_event_data_t` | Emitted when cloning a linked repository completes |
/// | `LORE_EVENT_LINK_BRANCH_CREATE` | `lore_link_branch_create_event_data_t` | Emitted when branching is enabled, reporting whether the link's branch was created or an existing one reused |
/// | `LORE_EVENT_LINK_CHANGE` | `lore_link_change_event_data_t` | Emitted when the link has been added and saved |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_add(
    globals: &LoreGlobalArgs,
    args: &LoreLinkAddArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::link::add)
}

/// Asynchronous version of `lore_link_add`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_CLONE_BEGIN` | `lore_repository_clone_begin_event_data_t` | Emitted when cloning a linked repository begins |
/// | `LORE_EVENT_REPOSITORY_CLONE_END` | `lore_repository_clone_end_event_data_t` | Emitted when cloning a linked repository completes |
/// | `LORE_EVENT_LINK_BRANCH_CREATE` | `lore_link_branch_create_event_data_t` | Emitted when branching is enabled, reporting whether the link's branch was created or an existing one reused |
/// | `LORE_EVENT_LINK_CHANGE` | `lore_link_change_event_data_t` | Emitted when the link has been added and saved |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_add_async(
    globals: &LoreGlobalArgs,
    args: &LoreLinkAddArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::link::add);
}

pub type LoreLinkRemoveArgs = crate::link::LoreLinkRemoveArgs;

/// Remove a link to another repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_CHANGE` | `lore_link_change_event_data_t` | Emitted when the link has been removed |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_remove(
    globals: &LoreGlobalArgs,
    args: &LoreLinkRemoveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::link::remove)
}

/// Asynchronous version of `lore_link_remove`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_CHANGE` | `lore_link_change_event_data_t` | Emitted when the link has been removed |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_remove_async(
    globals: &LoreGlobalArgs,
    args: &LoreLinkRemoveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::link::remove);
}

pub type LoreLinkInfoArgs = crate::link::LoreLinkInfoArgs;

/// Report detailed information about a single repository link.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_INFO` | `lore_link_info_event_data_t` | Emitted once for the described link |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_info(
    globals: &LoreGlobalArgs,
    args: &LoreLinkInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::link::info)
}

/// Asynchronous version of `lore_link_info`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_INFO` | `lore_link_info_event_data_t` | Emitted once for the described link |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreLinkInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::link::info);
}

pub type LoreLinkListArgs = crate::link::LoreLinkListArgs;

/// List all repository links configured in the current repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_ENTRY` | `lore_link_entry_event_data_t` | Emitted for each linked repository |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_list(
    globals: &LoreGlobalArgs,
    args: &LoreLinkListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::link::list)
}

/// Asynchronous version of `lore_link_list`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_ENTRY` | `lore_link_entry_event_data_t` | Emitted for each linked repository |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreLinkListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::link::list);
}

pub type LoreLinkUpdateArgs = crate::link::LoreLinkUpdateArgs;

/// Update properties of an existing repository link.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_CHANGE` | `lore_link_change_event_data_t` | Emitted when a link property is updated or finalized |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_update(
    globals: &LoreGlobalArgs,
    args: &LoreLinkUpdateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::link::update)
}

/// Asynchronous version of `lore_link_update`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Link Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LINK_CHANGE` | `lore_link_change_event_data_t` | Emitted when a link property is updated or finalized |
#[unsafe(no_mangle)]
pub extern "C" fn lore_link_update_async(
    globals: &LoreGlobalArgs,
    args: &LoreLinkUpdateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::link::update);
}

pub type LoreRepositoryCloneArgs = crate::repository::LoreRepositoryCloneArgs;

/// Clone a remote repository to a local path.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_CLONE_BEGIN` | `lore_repository_clone_begin_event_data_t` | Emitted when clone begins, includes remote URL and target path |
/// | `LORE_EVENT_REPOSITORY_CLONE_PROGRESS` | `lore_repository_clone_progress_event_data_t` | Emitted periodically during clone with progress data |
/// | `LORE_EVENT_REPOSITORY_CLONE_END` | `lore_repository_clone_end_event_data_t` | Emitted when clone completes successfully |
/// | `LORE_EVENT_REVISION_SYNC_TARGET` | `lore_revision_sync_target_event_data_t` | Emitted after resolving the target revision to sync during clone |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file written during initial sync |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted periodically during initial file sync |
/// | `LORE_EVENT_REVISION_SYNC_REVISION` | `lore_revision_sync_revision_event_data_t` | Emitted with the resulting revision |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view filters |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written to the local store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_clone(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryCloneArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::clone)
}

/// Asynchronous version of `lore_repository_clone`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_CLONE_BEGIN` | `lore_repository_clone_begin_event_data_t` | Emitted when clone begins, includes remote URL and target path |
/// | `LORE_EVENT_REPOSITORY_CLONE_PROGRESS` | `lore_repository_clone_progress_event_data_t` | Emitted periodically during clone with progress data |
/// | `LORE_EVENT_REPOSITORY_CLONE_END` | `lore_repository_clone_end_event_data_t` | Emitted when clone completes successfully |
/// | `LORE_EVENT_REVISION_SYNC_TARGET` | `lore_revision_sync_target_event_data_t` | Emitted after resolving the target revision to sync during clone |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file written during initial sync |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted periodically during initial file sync |
/// | `LORE_EVENT_REVISION_SYNC_REVISION` | `lore_revision_sync_revision_event_data_t` | Emitted with the resulting revision |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view filters |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written to the local store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_clone_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryCloneArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::clone);
}

pub type LoreRepositoryInfoArgs = crate::repository::LoreRepositoryInfoArgs;

/// Retrieve metadata about the current repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_DATA` | `lore_repository_data_event_data_t` | Emitted with repository metadata (name, URL, branch info, etc.) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_info(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::info)
}

/// Asynchronous version of `lore_repository_info`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_DATA` | `lore_repository_data_event_data_t` | Emitted with repository metadata (name, URL, branch info, etc.) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::info);
}

pub type LoreRepositoryDumpArgs = crate::repository::LoreRepositoryDumpArgs;

/// Dump the internal state of the repository for diagnostic purposes.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_DUMP_BEGIN` | `lore_repository_dump_begin_event_data_t` | Emitted before dump output begins |
/// | `LORE_EVENT_REPOSITORY_DUMP_END` | `lore_repository_dump_end_event_data_t` | Emitted when dump completes |
/// | `LORE_EVENT_REPOSITORY_STATE_DUMP` | `lore_repository_state_dump_event_data_t` | Emitted with repository state summary |
/// | `LORE_EVENT_REPOSITORY_STATE_DUMP_NODE` | `lore_repository_state_dump_node_event_data_t` | Emitted for each node in the state tree |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_dump(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryDumpArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::dump)
}

/// Asynchronous version of `lore_repository_dump`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_DUMP_BEGIN` | `lore_repository_dump_begin_event_data_t` | Emitted before dump output begins |
/// | `LORE_EVENT_REPOSITORY_DUMP_END` | `lore_repository_dump_end_event_data_t` | Emitted when dump completes |
/// | `LORE_EVENT_REPOSITORY_STATE_DUMP` | `lore_repository_state_dump_event_data_t` | Emitted with repository state summary |
/// | `LORE_EVENT_REPOSITORY_STATE_DUMP_NODE` | `lore_repository_state_dump_node_event_data_t` | Emitted for each node in the state tree |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_dump_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryDumpArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::dump);
}

pub type LoreRepositoryCreateArgs = crate::repository::LoreRepositoryCreateArgs;
pub type LoreRepositoryCreateMetadata = crate::repository::LoreRepositoryCreateMetadata;

/// Create a new Lore repository on the remote server.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_CREATE` | `lore_repository_create_event_data_t` | Emitted when the repository has been successfully created |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_create(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryCreateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::create)
}

/// Asynchronous version of `lore_repository_create`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_CREATE` | `lore_repository_create_event_data_t` | Emitted when the repository has been successfully created |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_create_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryCreateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::create);
}

pub type LoreRepositoryFlushArgs = crate::repository::LoreRepositoryFlushArgs;

/// Flush pending repository state to persistent storage.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_flush(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryFlushArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::flush)
}

/// Asynchronous version of `lore_repository_flush`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_flush_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryFlushArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::flush);
}

pub type LoreRepositoryGcArgs = crate::repository::LoreRepositoryGcArgs;

/// Run garbage collection to reclaim unreferenced storage in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_gc(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryGcArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::gc)
}

/// Asynchronous version of `lore_repository_gc`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_gc_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryGcArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::gc);
}

pub type LoreRepositoryReleaseArgs = crate::repository::LoreRepositoryReleaseArgs;

/// Release all cached store references for the given repository path.
///
/// Frees in-memory store data and releases file-backed store cache entries.
/// Any active repository contexts for this path remain valid, but once they
/// are dropped the stores will be freed. Subsequent opens will create fresh stores.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_release(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryReleaseArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::release)
}

/// Asynchronous version of `lore_repository_release`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_release_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryReleaseArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::release);
}

pub type LoreLayerAddArgs = crate::layer::LoreLayerAddArgs;

/// Add a new layer to the repository configuration.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Layer Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LAYER_ADD` | `lore_layer_add_event_data_t` | Emitted when a layer has been successfully added |
#[unsafe(no_mangle)]
pub extern "C" fn lore_layer_add(
    globals: &LoreGlobalArgs,
    args: &LoreLayerAddArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::layer::layer_add)
}

/// Asynchronous version of `lore_layer_add`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Layer Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LAYER_ADD` | `lore_layer_add_event_data_t` | Emitted when a layer has been successfully added |
#[unsafe(no_mangle)]
pub extern "C" fn lore_layer_add_async(
    globals: &LoreGlobalArgs,
    args: &LoreLayerAddArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::layer::layer_add);
}

pub type LoreLayerRemoveArgs = crate::layer::LoreLayerRemoveArgs;

/// Remove a layer from the repository configuration.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_layer_remove(
    globals: &LoreGlobalArgs,
    args: &LoreLayerRemoveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::layer::layer_remove)
}

/// Asynchronous version of `lore_layer_remove`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_layer_remove_async(
    globals: &LoreGlobalArgs,
    args: &LoreLayerRemoveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::layer::layer_remove);
}

pub type LoreLayerListArgs = crate::layer::LoreLayerListArgs;

/// List all layers configured in the repository.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Layer Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LAYER_ENTRY` | `lore_layer_entry_event_data_t` | Emitted for each layer configured in the repository |
#[unsafe(no_mangle)]
pub extern "C" fn lore_layer_list(
    globals: &LoreGlobalArgs,
    args: &LoreLayerListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::layer::layer_list)
}

/// Asynchronous version of `lore_layer_list`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Layer Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LAYER_ENTRY` | `lore_layer_entry_event_data_t` | Emitted for each layer configured in the repository |
#[unsafe(no_mangle)]
pub extern "C" fn lore_layer_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreLayerListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::layer::layer_list);
}

pub type LoreRepositoryListArgs = crate::repository::LoreRepositoryListArgs;

/// List all repositories available on the remote server.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_LIST_ENTRY` | `lore_repository_list_entry_event_data_t` | Emitted for each repository found |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_list(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::list)
}

/// Asynchronous version of `lore_repository_list`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_LIST_ENTRY` | `lore_repository_list_entry_event_data_t` | Emitted for each repository found |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::list);
}

pub type LoreRepositoryStatusArgs = crate::repository::LoreRepositoryStatusArgs;

/// Show the working directory status, including staged, dirty, and conflicted files.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_STATUS_REVISION` | `lore_repository_status_revision_event_data_t` | Emitted with current and staged revision info |
/// | `LORE_EVENT_REPOSITORY_STATUS_FILE` | `lore_repository_status_file_event_data_t` | Emitted for each file with pending changes, conflict status, or untracked status |
/// | `LORE_EVENT_PATH_IGNORE` | `lore_path_ignore_event_data_t` | Emitted for each path excluded by ignore rules |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_status(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryStatusArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::status)
}

/// Asynchronous version of `lore_repository_status`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_STATUS_REVISION` | `lore_repository_status_revision_event_data_t` | Emitted with current and staged revision info |
/// | `LORE_EVENT_REPOSITORY_STATUS_FILE` | `lore_repository_status_file_event_data_t` | Emitted for each file with pending changes, conflict status, or untracked status |
/// | `LORE_EVENT_PATH_IGNORE` | `lore_path_ignore_event_data_t` | Emitted for each path excluded by ignore rules |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_status_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryStatusArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::status);
}

pub type LoreRepositoryStoreImmutableQueryArgs =
    crate::repository::LoreRepositoryStoreImmutableQueryArgs;

/// Query the repository's immutable fragment store.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_STORE_IMMUTABLE_QUERY` | `lore_repository_store_immutable_query_event_data_t` | Emitted for each fragment entry found in the immutable store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_store_immutable_query(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryStoreImmutableQueryArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::repository::store_immutable_query,
    )
}

/// Asynchronous version of `lore_repository_store_immutable_query`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_STORE_IMMUTABLE_QUERY` | `lore_repository_store_immutable_query_event_data_t` | Emitted for each fragment entry found in the immutable store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_store_immutable_query_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryStoreImmutableQueryArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::repository::store_immutable_query,
    );
}

pub type LoreRepositoryVerifyStateArgs = crate::repository::LoreRepositoryVerifyStateArgs;
pub type LoreRepositoryVerifyFragmentArgs = crate::repository::LoreRepositoryVerifyFragmentArgs;

/// Verify the integrity of the repository's stored fragments.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_VERIFY_STATE_BEGIN` | `lore_repository_verify_state_begin_event_data_t` | Emitted when verify begins |
/// | `LORE_EVENT_REPOSITORY_VERIFY_STATE_END` | `lore_repository_verify_state_end_event_data_t` | Emitted when verify completes (success or with errors) |
/// | `LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT` | `lore_repository_verify_fragment_event_data_t` | Emitted for each fragment verified in the local store |
/// | `LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT_REMOTE` | `lore_repository_verify_fragment_remote_event_data_t` | Emitted for each fragment verified against the remote store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_verify_state(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryVerifyStateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::verify_state)
}

/// Asynchronous version of `lore_repository_verify_state`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Repository Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REPOSITORY_VERIFY_STATE_BEGIN` | `lore_repository_verify_state_begin_event_data_t` | Emitted when verify begins |
/// | `LORE_EVENT_REPOSITORY_VERIFY_STATE_END` | `lore_repository_verify_state_end_event_data_t` | Emitted when verify completes (success or with errors) |
/// | `LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT` | `lore_repository_verify_fragment_event_data_t` | Emitted for each fragment verified in the local store |
/// | `LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT_REMOTE` | `lore_repository_verify_fragment_remote_event_data_t` | Emitted for each fragment verified against the remote store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_verify_state_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryVerifyStateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::verify_state);
}

pub type LoreRevisionCommitArgs = crate::revision::LoreRevisionCommitArgs;

/// Commit staged files to create a new revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when commit begins fragmenting files |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted periodically during commit with file processing counts |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when commit file processing completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revision details (hash, branch, parents) |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the committed revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written or deduplicated |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_commit(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionCommitArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::commit)
}

/// Asynchronous version of `lore_revision_commit`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when commit begins fragmenting files |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted periodically during commit with file processing counts |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when commit file processing completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revision details (hash, branch, parents) |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the committed revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written or deduplicated |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_commit_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionCommitArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::commit);
}

pub type LoreRevisionAmendArgs = crate::revision::LoreRevisionAmendArgs;

/// Amend the most recent revision with updated metadata.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the amended revision details |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the amended revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_amend(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionAmendArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::amend)
}

/// Asynchronous version of `lore_revision_amend`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the amended revision details |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata entry of the amended revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_amend_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionAmendArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::amend);
}

pub type LoreRevisionInfoArgs = crate::revision::LoreRevisionInfoArgs;

/// Retrieve metadata and delta information about a specific revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_INFO` | `lore_revision_info_event_data_t` | Emitted with revision metadata (hash, branch, parents, file count, etc.) |
/// | `LORE_EVENT_REVISION_INFO_DELTA` | `lore_revision_info_delta_event_data_t` | Emitted with delta information between revision and its parent (when delta=true) |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata key/value of the revision (when metadata=true) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_info(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::info)
}

/// Asynchronous version of `lore_revision_info`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_INFO` | `lore_revision_info_event_data_t` | Emitted with revision metadata (hash, branch, parents, file count, etc.) |
/// | `LORE_EVENT_REVISION_INFO_DELTA` | `lore_revision_info_delta_event_data_t` | Emitted with delta information between revision and its parent (when delta=true) |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata key/value of the revision (when metadata=true) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::info);
}

pub type LoreRevisionDiffArgs = crate::revision::LoreRevisionDiffArgs;

/// Show files that differ between two revisions.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_DIFF_FILE` | `lore_revision_diff_file_event_data_t` | Emitted for each file that differs between the two revisions |
/// | `LORE_EVENT_REVISION_RESOLVE` | `lore_revision_resolve_event_data_t` | Emitted when resolving a partial or numbered revision reference |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_diff(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionDiffArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::diff)
}

/// Asynchronous version of `lore_revision_diff`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_DIFF_FILE` | `lore_revision_diff_file_event_data_t` | Emitted for each file that differs between the two revisions |
/// | `LORE_EVENT_REVISION_RESOLVE` | `lore_revision_resolve_event_data_t` | Emitted when resolving a partial or numbered revision reference |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_diff_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionDiffArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::diff);
}

pub type LoreRevisionFindArgs = crate::revision::LoreRevisionFindArgs;

/// Find a revision by metadata or revision number.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_FIND` | `lore_revision_find_event_data_t` | Emitted when a matching revision is found (exact or partial match) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_find(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionFindArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::find)
}

/// Asynchronous version of `lore_revision_find`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_FIND` | `lore_revision_find_event_data_t` | Emitted when a matching revision is found (exact or partial match) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_find_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionFindArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::find);
}

pub type LoreRevisionHistoryArgs = crate::revision::LoreRevisionHistoryArgs;

/// Retrieve the commit history of the current branch.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_HISTORY` | `lore_revision_history_event_data_t` | Emitted once with summary info before entries stream |
/// | `LORE_EVENT_REVISION_HISTORY_ENTRY` | `lore_revision_history_entry_event_data_t` | Emitted for each revision in the history |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_history(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionHistoryArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::history)
}

/// Asynchronous version of `lore_revision_history`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_HISTORY` | `lore_revision_history_event_data_t` | Emitted once with summary info before entries stream |
/// | `LORE_EVENT_REVISION_HISTORY_ENTRY` | `lore_revision_history_entry_event_data_t` | Emitted for each revision in the history |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_history_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionHistoryArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::history);
}

pub type LoreRevisionRestoreArgs = crate::revision::LoreRevisionRestoreArgs;

/// Restore the working directory to a previously committed revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_RESTORE_FILE_BEGIN` | `lore_revision_restore_file_begin_event_data_t` | Emitted when restore starts processing files |
/// | `LORE_EVENT_REVISION_RESTORE_FILE` | `lore_revision_restore_file_event_data_t` | Emitted for each file being restored |
/// | `LORE_EVENT_REVISION_RESTORE_FILE_END` | `lore_revision_restore_file_end_event_data_t` | Emitted when file processing completes |
/// | `LORE_EVENT_REVISION_RESTORE_FRAGMENT_BEGIN` | `lore_revision_restore_fragment_begin_event_data_t` | Emitted when fragment download begins for a file |
/// | `LORE_EVENT_REVISION_RESTORE_FRAGMENT_PROGRESS` | `lore_revision_restore_fragment_progress_event_data_t` | Emitted periodically during fragment download |
/// | `LORE_EVENT_REVISION_RESTORE_FRAGMENT_END` | `lore_revision_restore_fragment_end_event_data_t` | Emitted when fragment download completes |
/// | `LORE_EVENT_REVISION_RESTORE_REVISION` | `lore_revision_restore_revision_event_data_t` | Emitted with the restored revision details |
/// | `LORE_EVENT_REVISION_RESTORE_SYNC_BEGIN` | `lore_revision_restore_sync_begin_event_data_t` | Emitted when starting to apply the changes on the target state |
/// | `LORE_EVENT_REVISION_RESTORE_SYNC_END` | `lore_revision_restore_sync_end_event_data_t` | Emitted after applying the changes on the target state is complete |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit of restored revision starts |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed restored revision |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during changes realization |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for metadata of the restored revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for fragments written during restore commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_restore(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRestoreArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::restore)
}

/// Asynchronous version of `lore_revision_restore`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_RESTORE_FILE_BEGIN` | `lore_revision_restore_file_begin_event_data_t` | Emitted when restore starts processing files |
/// | `LORE_EVENT_REVISION_RESTORE_FILE` | `lore_revision_restore_file_event_data_t` | Emitted for each file being restored |
/// | `LORE_EVENT_REVISION_RESTORE_FILE_END` | `lore_revision_restore_file_end_event_data_t` | Emitted when file processing completes |
/// | `LORE_EVENT_REVISION_RESTORE_FRAGMENT_BEGIN` | `lore_revision_restore_fragment_begin_event_data_t` | Emitted when fragment download begins for a file |
/// | `LORE_EVENT_REVISION_RESTORE_FRAGMENT_PROGRESS` | `lore_revision_restore_fragment_progress_event_data_t` | Emitted periodically during fragment download |
/// | `LORE_EVENT_REVISION_RESTORE_FRAGMENT_END` | `lore_revision_restore_fragment_end_event_data_t` | Emitted when fragment download completes |
/// | `LORE_EVENT_REVISION_RESTORE_REVISION` | `lore_revision_restore_revision_event_data_t` | Emitted with the restored revision details |
/// | `LORE_EVENT_REVISION_RESTORE_SYNC_BEGIN` | `lore_revision_restore_sync_begin_event_data_t` | Emitted when sync begins |
/// | `LORE_EVENT_REVISION_RESTORE_SYNC_END` | `lore_revision_restore_sync_end_event_data_t` | Emitted when sync completes |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit of restored revision starts |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed restored revision |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during changes realization |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for metadata of the restored revision |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for fragments written during restore commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_restore_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRestoreArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::restore);
}

pub type LoreRevisionMetadataClearArgs = crate::revision::LoreRevisionMetadataClearArgs;

/// Clear all metadata from the current revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA_CLEAR_REVISION` | `lore_metadata_clear_revision_event_data_t` | Emitted when metadata has been cleared for the current revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_clear(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::metadata_clear)
}

/// Asynchronous version of `lore_revision_metadata_clear`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA_CLEAR_REVISION` | `lore_metadata_clear_revision_event_data_t` | Emitted when metadata has been cleared for the current revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_clear_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::metadata_clear);
}

pub type LoreRevisionMetadataGetArgs = crate::revision::LoreRevisionMetadataGetArgs;

/// Get a specific metadata key/value pair from the current revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted with the requested key/value for the revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_get(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::metadata_get)
}

/// Asynchronous version of `lore_revision_metadata_get`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted with the requested key/value for the revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::metadata_get);
}

pub type LoreRevisionMetadataListArgs = crate::revision::LoreRevisionMetadataListArgs;

/// List all metadata key/value pairs associated with the current revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata key/value associated with the revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_list(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::metadata_list)
}

/// Asynchronous version of `lore_revision_metadata_list`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revision Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for each metadata key/value associated with the revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::metadata_list);
}

pub type LoreRevisionMetadataSetArgs = crate::revision::LoreRevisionMetadataSetArgs;

/// Set a metadata key/value pair on the current revision.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_set(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::metadata_set)
}

/// Asynchronous version of `lore_revision_metadata_set`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_metadata_set_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::metadata_set);
}

pub type LoreRevisionSyncArgs = crate::revision::LoreRevisionSyncArgs;

/// Synchronize the working directory to a target revision, optionally merging divergent branches.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Sync Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_SYNC_TARGET` | `lore_revision_sync_target_event_data_t` | Emitted once after resolving the target revision with source/target revision info, branch, and remote URL |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file deleted, modified, added, or merged during sync |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted periodically during file realization and once at completion with cumulative update/delete/automerge/conflict counts |
/// | `LORE_EVENT_REVISION_SYNC_REVISION` | `lore_revision_sync_revision_event_data_t` | Emitted once at the end with the resulting revision, branch, and merge/conflict flags |
/// | `LORE_EVENT_REVISION_RESOLVE` | `lore_revision_resolve_event_data_t` | Emitted when resolving a partial or numbered revision reference |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view or ignore filters |
/// | `LORE_EVENT_BRANCH_MERGE_START_BEGIN` | `lore_branch_merge_start_begin_event_data_t` | Emitted when an auto-merge is initiated (diverged branches) |
/// | `LORE_EVENT_BRANCH_MERGE_START_END` | `lore_branch_merge_start_end_event_data_t` | Emitted when the auto-merge operation completes |
/// | `LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE` | `lore_branch_merge_conflict_file_event_data_t` | Emitted for each file with an unresolved merge conflict |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-merge auto-commits (no conflicts) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed merge revision |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for metadata of the auto-merge commit |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written during auto-merge commit |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged for deletion during merge realization |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_sync(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionSyncArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::sync)
}

/// Asynchronous version of `lore_revision_sync`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Sync Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVISION_SYNC_TARGET` | `lore_revision_sync_target_event_data_t` | Emitted once after resolving the target revision with source/target revision info, branch, and remote URL |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file deleted, modified, added, or merged during sync |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted periodically during file realization and once at completion with cumulative update/delete/automerge/conflict counts |
/// | `LORE_EVENT_REVISION_SYNC_REVISION` | `lore_revision_sync_revision_event_data_t` | Emitted once at the end with the resulting revision, branch, and merge/conflict flags |
/// | `LORE_EVENT_REVISION_RESOLVE` | `lore_revision_resolve_event_data_t` | Emitted when resolving a partial or numbered revision reference |
/// | `LORE_EVENT_FILTER_EXCLUDE` | `lore_filter_exclude_event_data_t` | Emitted for each path excluded by view or ignore filters |
/// | `LORE_EVENT_BRANCH_MERGE_START_BEGIN` | `lore_branch_merge_start_begin_event_data_t` | Emitted when an auto-merge is initiated (diverged branches) |
/// | `LORE_EVENT_BRANCH_MERGE_START_END` | `lore_branch_merge_start_end_event_data_t` | Emitted when the auto-merge operation completes |
/// | `LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE` | `lore_branch_merge_conflict_file_event_data_t` | Emitted for each file with an unresolved merge conflict |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-merge auto-commits (no conflicts) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed merge revision |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for metadata of the auto-merge commit |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for each fragment written during auto-merge commit |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged for deletion during merge realization |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_sync_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionSyncArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::sync);
}

pub type LoreRevisionRevertArgs = crate::revision::LoreRevisionRevertArgs;

/// Revert a revision, applying its inverse changes to the working tree.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_START_BEGIN` | `lore_revert_start_begin_event_data_t` | Emitted when revert begins, includes target revision info |
/// | `LORE_EVENT_REVERT_START_END` | `lore_revert_start_end_event_data_t` | Emitted when revert completes, includes conflict flag |
/// | `LORE_EVENT_REVERT_CONFLICT_FILE` | `lore_revert_conflict_file_event_data_t` | Emitted for each file with an unresolved revert conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during apply_diff phase |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file modified during revert realization |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged for deletion during revert |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit starts (no conflicts) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revert revision |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for metadata of the auto-commit |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for fragments written during auto-commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::revert)
}

/// Asynchronous version of `lore_revision_revert`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_START_BEGIN` | `lore_revert_start_begin_event_data_t` | Emitted when revert begins, includes target revision info |
/// | `LORE_EVENT_REVERT_START_END` | `lore_revert_start_end_event_data_t` | Emitted when revert completes, includes conflict flag |
/// | `LORE_EVENT_REVERT_CONFLICT_FILE` | `lore_revert_conflict_file_event_data_t` | Emitted for each file with an unresolved revert conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during apply_diff phase |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file modified during revert realization |
/// | `LORE_EVENT_FILE_STAGE_FILE` | `lore_file_stage_file_event_data_t` | Emitted for each file staged for deletion during revert |
/// | `LORE_EVENT_REVISION_COMMIT_BEGIN` | `lore_revision_commit_begin_event_data_t` | Emitted when auto-commit starts (no conflicts) |
/// | `LORE_EVENT_REVISION_COMMIT_PROGRESS` | `lore_revision_commit_progress_event_data_t` | Emitted during auto-commit |
/// | `LORE_EVENT_REVISION_COMMIT_END` | `lore_revision_commit_end_event_data_t` | Emitted when auto-commit completes |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION` | `lore_revision_commit_revision_event_data_t` | Emitted with the committed revert revision |
/// | `LORE_EVENT_METADATA` | `lore_metadata_event_data_t` | Emitted for metadata of the auto-commit |
/// | `LORE_EVENT_FRAGMENT_WRITE` | `lore_fragment_write_event_data_t` | Emitted for fragments written during auto-commit |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::revert);
}

pub type LoreRevisionRevertAbortArgs = crate::revision::LoreRevisionRevertAbortArgs;

/// Abort an in-progress revert operation and restore the previous state.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_ABORT_BEGIN` | `lore_revert_abort_begin_event_data_t` | Emitted when revert abort begins |
/// | `LORE_EVENT_REVERT_ABORT_END` | `lore_revert_abort_end_event_data_t` | Emitted when revert abort completes |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization while reverting |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_abort(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertAbortArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::revert_abort)
}

/// Asynchronous version of `lore_revision_revert_abort`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_ABORT_BEGIN` | `lore_revert_abort_begin_event_data_t` | Emitted when revert abort begins |
/// | `LORE_EVENT_REVERT_ABORT_END` | `lore_revert_abort_end_event_data_t` | Emitted when revert abort completes |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization while reverting |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_abort_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertAbortArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::revert_abort);
}

pub type LoreRevisionRevertUnresolveArgs = crate::revision::LoreRevisionRevertUnresolveArgs;

/// Mark conflicting files in a revert operation as unresolved.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_UNRESOLVE_FILE` | `lore_revert_unresolve_file_event_data_t` | Emitted for each file marked as unresolved |
/// | `LORE_EVENT_REVERT_UNRESOLVE_REVISION` | `lore_revert_unresolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_unresolve(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertUnresolveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::revert_unresolve)
}

/// Asynchronous version of `lore_revision_revert_unresolve`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_UNRESOLVE_FILE` | `lore_revert_unresolve_file_event_data_t` | Emitted for each file marked as unresolved |
/// | `LORE_EVENT_REVERT_UNRESOLVE_REVISION` | `lore_revert_unresolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_unresolve_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertUnresolveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::revert_unresolve);
}

pub type LoreRevisionRevertRestartArgs = crate::revision::LoreRevisionRevertRestartArgs;

/// Restart a revert operation, re-materializing files with conflicts.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_CONFLICT_FILE` | `lore_revert_conflict_file_event_data_t` | Emitted for each file with a remaining revert conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file re-materialized |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_restart(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertRestartArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::revert_restart)
}

/// Asynchronous version of `lore_revision_revert_restart`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_CONFLICT_FILE` | `lore_revert_conflict_file_event_data_t` | Emitted for each file with a remaining revert conflict |
/// | `LORE_EVENT_REVISION_SYNC_PROGRESS` | `lore_revision_sync_progress_event_data_t` | Emitted during file realization |
/// | `LORE_EVENT_REVISION_SYNC_FILE` | `lore_revision_sync_file_event_data_t` | Emitted for each file re-materialized |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_restart_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertRestartArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::revert_restart);
}

pub type LoreRevisionRevertResolveArgs = crate::revision::LoreRevisionRevertResolveArgs;

/// Resolve a revert conflict by marking conflicting files as resolved.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_RESOLVE_FILE` | `lore_revert_resolve_file_event_data_t` | Emitted for each file marked as resolved |
/// | `LORE_EVENT_REVERT_RESOLVE_REVISION` | `lore_revert_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_resolve(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertResolveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision::revert_resolve)
}

/// Asynchronous version of `lore_revision_revert_resolve`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_RESOLVE_FILE` | `lore_revert_resolve_file_event_data_t` | Emitted for each file marked as resolved |
/// | `LORE_EVENT_REVERT_RESOLVE_REVISION` | `lore_revert_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_resolve_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertResolveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision::revert_resolve);
}

pub type LoreRevisionRevertResolveMineArgs = crate::revision::LoreRevisionRevertResolveMineArgs;

/// Resolve a revert conflict by accepting the "mine" version of each conflicting file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_RESOLVE_FILE` | `lore_revert_resolve_file_event_data_t` | Emitted for each file resolved by keeping "mine" |
/// | `LORE_EVENT_REVERT_RESOLVE_REVISION` | `lore_revert_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_resolve_mine(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertResolveMineArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision::revert_resolve_mine,
    )
}

/// Asynchronous version of `lore_revision_revert_resolve_mine`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_RESOLVE_FILE` | `lore_revert_resolve_file_event_data_t` | Emitted for each file resolved by keeping "mine" |
/// | `LORE_EVENT_REVERT_RESOLVE_REVISION` | `lore_revert_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_resolve_mine_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertResolveMineArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision::revert_resolve_mine,
    );
}

pub type LoreRevisionRevertResolveTheirsArgs = crate::revision::LoreRevisionRevertResolveTheirsArgs;

/// Resolve a revert conflict by accepting the "theirs" version of each conflicting file.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_RESOLVE_FILE` | `lore_revert_resolve_file_event_data_t` | Emitted for each file resolved by keeping "theirs" |
/// | `LORE_EVENT_REVERT_RESOLVE_REVISION` | `lore_revert_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_resolve_theirs(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertResolveTheirsArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision::revert_resolve_theirs,
    )
}

/// Asynchronous version of `lore_revision_revert_resolve_theirs`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Revert Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_REVERT_RESOLVE_FILE` | `lore_revert_resolve_file_event_data_t` | Emitted for each file resolved by keeping "theirs" |
/// | `LORE_EVENT_REVERT_RESOLVE_REVISION` | `lore_revert_resolve_revision_event_data_t` | Emitted with the updated staged revision |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_revert_resolve_theirs_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionRevertResolveTheirsArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision::revert_resolve_theirs,
    );
}

pub type LoreSharedStoreCreateArgs = crate::shared_store::LoreSharedStoreCreateArgs;

/// Create a new shared store at the specified path.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Shared Store Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_SHARED_STORE_CREATE` | `lore_shared_store_create_event_data_t` | Emitted on success after the shared store is created, carrying the path of the newly created store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_shared_store_create(
    globals: &LoreGlobalArgs,
    args: &LoreSharedStoreCreateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::shared_store::create)
}

/// Create a new shared store at the specified path (async).
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Shared Store Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_SHARED_STORE_CREATE` | `lore_shared_store_create_event_data_t` | Emitted on success after the shared store is created, carrying the path of the newly created store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_shared_store_create_async(
    globals: &LoreGlobalArgs,
    args: &LoreSharedStoreCreateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::shared_store::create);
}

pub type LoreSharedStoreInfoArgs = crate::shared_store::LoreSharedStoreInfoArgs;

/// Retrieve the path of the configured default shared store.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Shared Store Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_SHARED_STORE_INFO` | `lore_shared_store_info_event_data_t` | Emitted on success carrying the path of the configured default shared store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_shared_store_info(
    globals: &LoreGlobalArgs,
    args: &LoreSharedStoreInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::shared_store::info)
}

/// Retrieve the path of the configured default shared store (async).
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Shared Store Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_SHARED_STORE_INFO` | `lore_shared_store_info_event_data_t` | Emitted on success carrying the path of the configured default shared store |
#[unsafe(no_mangle)]
pub extern "C" fn lore_shared_store_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreSharedStoreInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::shared_store::info);
}

pub type LoreSharedStoreSetUseAutomaticallyArgs =
    crate::shared_store::LoreSharedStoreSetUseAutomaticallyArgs;

/// Set whether to automatically use the shared store.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_shared_store_set_use_automatically(
    globals: &LoreGlobalArgs,
    args: &LoreSharedStoreSetUseAutomaticallyArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::shared_store::set_use_automatically,
    )
}

/// Set whether to automatically use the shared store (async).
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_shared_store_set_use_automatically_async(
    globals: &LoreGlobalArgs,
    args: &LoreSharedStoreSetUseAutomaticallyArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::shared_store::set_use_automatically,
    );
}

pub type LoreStorageOpenArgs = crate::storage::open::LoreStorageOpenArgs;

/// Open a content-addressed storage handle.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_OPENED` | `lore_storage_opened_event_data_t` | Emitted on success carrying the opened handle id |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status` is `0` on success or the error code on failure |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_open(
    globals: &LoreGlobalArgs,
    args: &LoreStorageOpenArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::open::open)
}

/// Open a content-addressed storage handle (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_open_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageOpenArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::open::open);
}

pub type LoreStoragePutItem = crate::storage::put::LoreStoragePutItem;
pub type LoreStoragePutArgs = crate::storage::put::LoreStoragePutArgs;

/// Store one or more content-addressed buffers.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_PUT_ITEM_COMPLETE` | `lore_storage_put_item_complete_event_data_t` | Emitted once per input item — success or failure; `stored_local`/`stored_remote` report where the content landed |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status` is `0` iff every item succeeded, else the error code |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_put(
    globals: &LoreGlobalArgs,
    args: &LoreStoragePutArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::put::put)
}

/// Store one or more content-addressed buffers (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_put_async(
    globals: &LoreGlobalArgs,
    args: &LoreStoragePutArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::put::put);
}

pub type LoreStorageGetItem = crate::storage::get::LoreStorageGetItem;
pub type LoreStorageGetArgs = crate::storage::get::LoreStorageGetArgs;

/// Read one or more content-addressed buffers.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_GET_HEADER` | `lore_storage_get_header_event_data_t` | Size of the item's reassembled content, emitted before any DATA events |
/// | `LORE_EVENT_STORAGE_GET_DATA` | `lore_storage_get_data_event_data_t` | Payload bytes — valid only during the callback invocation |
/// | `LORE_EVENT_STORAGE_GET_ITEM_COMPLETE` | `lore_storage_get_item_complete_event_data_t` | Terminal per-item event |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status` is `0` iff every item succeeded, else the error code |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::get::get)
}

/// Read one or more content-addressed buffers (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::get::get);
}

pub type LoreStorageGetResolvedItem = crate::storage::get_resolved::LoreStorageGetResolvedItem;
pub type LoreStorageGetResolvedArgs = crate::storage::get_resolved::LoreStorageGetResolvedArgs;

/// Resolve one or more mutable keys and read the content they name, in one round trip.
///
/// `lore_storage_mutable_load` followed by `lore_storage_get`, performed by the server. The keys
/// are read under the `LORE_KEY_TYPE_RESOLVE` key type, which is what `lore_storage_put_resolved`
/// publishes; no other key type is resolvable this way.
///
/// The `address` in every event is the *resolved* address, so a caller can learn the key-to-hash
/// mapping from the event stream. A key with no mapping, or one naming absent content, reports
/// `error_code = ADDRESS_NOT_FOUND`, and the terminal event then carries a zero address.
///
/// Set `streaming` to receive one `LORE_EVENT_STORAGE_GET_DATA` per leaf fragment instead of a
/// single reassembled buffer, exactly as `lore_storage_get` does. Without it the whole content is
/// materialised in memory before the first byte reaches the callback, so a key naming something
/// large should set it.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_GET_HEADER` | `lore_storage_get_header_event_data_t` | Size of the item's reassembled content, emitted before any DATA events |
/// | `LORE_EVENT_STORAGE_GET_DATA` | `lore_storage_get_data_event_data_t` | Payload bytes — valid only during the callback invocation. One event per item, or one per leaf fragment when `streaming` is set |
/// | `LORE_EVENT_STORAGE_GET_ITEM_COMPLETE` | `lore_storage_get_item_complete_event_data_t` | Terminal per-item event |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status` is `0` iff every item succeeded, else the error code |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_resolved(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetResolvedArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::get_resolved::get_resolved,
    )
}

/// Resolve one or more mutable keys and read the content they name (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_resolved_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetResolvedArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::get_resolved::get_resolved,
    );
}

pub type LoreStoragePutResolvedItem = crate::storage::put_resolved::LoreStoragePutResolvedItem;
pub type LoreStoragePutResolvedArgs = crate::storage::put_resolved::LoreStoragePutResolvedArgs;

/// Store one or more buffers and publish a mutable key naming each, in one round trip.
///
/// `lore_storage_put` followed by `lore_storage_mutable_store`, fused into one request when the
/// content fits a single fragment. The key is published under `LORE_KEY_TYPE_RESOLVE`, making it
/// readable by `lore_storage_get_resolved`, and the mapping is written only once the content is
/// stored — so a key published this way never resolves to content that is not there. Writing the
/// same key type directly with `lore_storage_mutable_store` carries no such guarantee.
///
/// The local store always receives both the content and the mapping. `remote_write = 1` also
/// publishes them remotely, matching `lore_storage_put`; there is no local-then-remote fallback.
/// A zero `key` or a zero `partition` rejects with `INVALID_ARGUMENTS`.
///
/// A zero-length `data` **removes** the key's mapping rather than publishing one: no content is
/// stored, the key is set to the zero hash, and `lore_storage_get_resolved` then reports
/// `ADDRESS_NOT_FOUND` for it. The terminal event carries the zero content hash and the caller's
/// context.
///
/// With `remote_write = 0` this evicts only the locally cached mapping. The local mutable store
/// is a cache, not an authority, so a key published remotely resolves again on the next call.
/// Deleting a published key requires `remote_write = 1`.
///
/// A remote content upload that fails still leaves a successful local write, so the key is not
/// published remotely and `stored_remote` is `0` while `error_code` stays `NONE`. Check
/// `stored_remote`, not `error_code`, to confirm the key is visible to other clients.
///
/// Publishing is last-writer-wins. Two callers publishing the same key concurrently both
/// succeed, and the key ends up naming whichever content was published second — the first
/// publisher is not told it was overwritten. Callers needing to detect a lost update should
/// store the content with `lore_storage_put` and publish with
/// `lore_storage_mutable_compare_and_swap`, which costs the second round trip this operation
/// exists to avoid.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_PUT_ITEM_COMPLETE` | `lore_storage_put_item_complete_event_data_t` | Emitted once per input item; `address` is the content the key now resolves to, and `stored_local`/`stored_remote` report where it landed |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status` is `0` iff every item succeeded, else the error code |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_put_resolved(
    globals: &LoreGlobalArgs,
    args: &LoreStoragePutResolvedArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::put_resolved::put_resolved,
    )
}

/// Store one or more buffers and publish a mutable key naming each (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_put_resolved_async(
    globals: &LoreGlobalArgs,
    args: &LoreStoragePutResolvedArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::put_resolved::put_resolved,
    );
}

pub type LoreStorageCloseArgs = crate::storage::close::LoreStorageCloseArgs;

/// Release a content-addressed storage handle.
///
/// Subsequent calls against the same handle return `InvalidArguments`.
/// Close does not block on the flush it spawns — `Complete` fires after
/// the in-flight counter drains, not after the flush finishes.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_close(
    globals: &LoreGlobalArgs,
    args: &LoreStorageCloseArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::close::close)
}

/// Release a content-addressed storage handle (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_close_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageCloseArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::close::close);
}

pub type LoreStorageFlushArgs = crate::storage::flush::LoreStorageFlushArgs;

/// Flush pending writes through the handle's stores.
///
/// On disk-backed stores this performs an fsync honoring `globals.sync_data`.
/// On in-memory stores the underlying flush is a no-op and the call still
/// completes with `status: 0`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_flush(
    globals: &LoreGlobalArgs,
    args: &LoreStorageFlushArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::flush::flush)
}

/// Flush pending writes through the handle's stores (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_flush_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageFlushArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::flush::flush);
}

pub type LoreStorageGetMetadataItem = crate::storage::get_metadata::LoreStorageGetMetadataItem;
pub type LoreStorageGetMetadataArgs = crate::storage::get_metadata::LoreStorageGetMetadataArgs;

/// Fetch fragment metadata for one or more `(partition, address)` pairs without paying the
/// payload bytes. Each item's terminal event carries the resolved `Fragment` (`flags`,
/// `size_payload`, `size_content`); on miss `error_code == ADDRESS_NOT_FOUND`.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_GET_METADATA_ITEM_COMPLETE` | `lore_storage_get_metadata_item_complete_event_data_t` | Per-item terminal event |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status` is `0` iff every item succeeded, else the error code |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_metadata(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetMetadataArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::get_metadata::get_metadata,
    )
}

/// Fetch fragment metadata for one or more addresses (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_metadata_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetMetadataArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::get_metadata::get_metadata,
    );
}

pub type LoreStorageObliterateArgs = crate::storage::obliterate::LoreStorageObliterateArgs;

/// Delete one or more `(partition, address)` entries from the store.
///
/// Idempotent on absent items; emits one `OBLITERATE_ITEM_COMPLETE` event
/// per item carrying `local_success` / `remote_success` / `error_code`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_obliterate(
    globals: &LoreGlobalArgs,
    args: &LoreStorageObliterateArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::obliterate::obliterate,
    )
}

/// Delete content (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_obliterate_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageObliterateArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::obliterate::obliterate,
    );
}

pub type LoreStorageMutableLoadItem = crate::storage::mutable_load::LoreStorageMutableLoadItem;
pub type LoreStorageMutableLoadArgs = crate::storage::mutable_load::LoreStorageMutableLoadArgs;

/// Read one or more mutable key values.
///
/// Each item acts on the local mutable store by default, or the remote mutable store when
/// `globals.remote` is set (or the handle was opened remote-bound), over the shared storage
/// session.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_MUTABLE_LOAD_ITEM_COMPLETE` | `lore_storage_mutable_load_item_complete_event_data_t` | Per-item terminal event carrying the value; `error_code == ADDRESS_NOT_FOUND` on a miss |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status: 0` iff every item succeeded |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_load(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableLoadArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_load::mutable_load,
    )
}

/// Read one or more mutable key values (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_load_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableLoadArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_load::mutable_load,
    );
}

pub type LoreStorageMutableStoreItem = crate::storage::mutable_store::LoreStorageMutableStoreItem;
pub type LoreStorageMutableStoreArgs = crate::storage::mutable_store::LoreStorageMutableStoreArgs;

/// Write one or more mutable key-value pairs. Storing the null value removes the key.
///
/// Each item acts on the local mutable store by default, or the remote mutable store when
/// `globals.remote` is set (or the handle was opened remote-bound), over the shared storage
/// session.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_MUTABLE_STORE_ITEM_COMPLETE` | `lore_storage_mutable_store_item_complete_event_data_t` | Per-item terminal event |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status: 0` iff every item succeeded |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_store(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableStoreArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_store::mutable_store,
    )
}

/// Write one or more mutable key-value pairs (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_store_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableStoreArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_store::mutable_store,
    );
}

pub type LoreStorageMutableCompareAndSwapItem =
    crate::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapItem;
pub type LoreStorageMutableCompareAndSwapArgs =
    crate::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapArgs;

/// Conditionally swap one or more mutable key values. Each item updates the key to `value` when
/// its current value matches `expected`, and reports the value the key held before the swap.
///
/// Each item acts on the local mutable store by default, or the remote mutable store when
/// `globals.remote` is set (or the handle was opened remote-bound), over the shared storage
/// session.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_MUTABLE_COMPARE_AND_SWAP_ITEM_COMPLETE` | `lore_storage_mutable_compare_and_swap_item_complete_event_data_t` | Per-item terminal event carrying `previous`; the swap took effect when `previous == expected` |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status: 0` iff every item succeeded |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_compare_and_swap(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableCompareAndSwapArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_compare_and_swap::mutable_compare_and_swap,
    )
}

/// Conditionally swap one or more mutable key values (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_compare_and_swap_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableCompareAndSwapArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_compare_and_swap::mutable_compare_and_swap,
    );
}

pub type LoreStorageMutableListItem = crate::storage::mutable_list::LoreStorageMutableListItem;
pub type LoreStorageMutableListArgs = crate::storage::mutable_list::LoreStorageMutableListArgs;

/// List the mutable key-value pairs of a given type for one or more partitions.
///
/// Acts on the local mutable store only; a remote-targeted call (`globals.remote`, or a
/// remote-bound handle) is rejected with `INVALID_ARGUMENTS`. A zero/default partition lists
/// every partition the caller can access.
///
/// # Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_STORAGE_MUTABLE_LIST_ENTRY` | `lore_storage_mutable_list_entry_event_data_t` | One `(key, value)` pair, emitted before the item's terminal event |
/// | `LORE_EVENT_STORAGE_MUTABLE_LIST_ITEM_COMPLETE` | `lore_storage_mutable_list_item_complete_event_data_t` | Per-item terminal event |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | `status: 0` iff every item succeeded |
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_list(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_list::mutable_list,
    )
}

/// List mutable key-value pairs (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_mutable_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageMutableListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::storage::mutable_list::mutable_list,
    );
}

pub type LoreStorageCopyArgs = crate::storage::copy::LoreStorageCopyArgs;

/// Copy content from one partition to another within the same store.
///
/// Same-partition source/target rejects with `INVALID_ARGUMENTS`. The
/// item's content hash is preserved; only the source address is carried
/// in the per-item event.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_copy(
    globals: &LoreGlobalArgs,
    args: &LoreStorageCopyArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::copy::copy)
}

/// Copy content (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_copy_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageCopyArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::copy::copy);
}

pub type LoreStoragePutFileArgs = crate::storage::put_file::LoreStoragePutFileArgs;

/// Read one or more files into the content-addressed store.
///
/// Each item emits `LORE_EVENT_STORAGE_PUT_ITEM_COMPLETE` carrying the
/// computed address. Empty files short-circuit to the zero-hash address
/// without opening for read.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_put_file(
    globals: &LoreGlobalArgs,
    args: &LoreStoragePutFileArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::put_file::put_file)
}

/// Read files into the store (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_put_file_async(
    globals: &LoreGlobalArgs,
    args: &LoreStoragePutFileArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::put_file::put_file);
}

pub type LoreStorageGetFileArgs = crate::storage::get_file::LoreStorageGetFileArgs;

/// Write content-addressed payloads to filesystem paths.
///
/// Each item emits `LORE_EVENT_STORAGE_GET_ITEM_COMPLETE`. No HEADER or
/// DATA events are produced — the payload is written straight to disk.
/// On partial-write failure the library leaves whatever state the
/// failure produced; cleanup is the caller's responsibility.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_file(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetFileArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::get_file::get_file)
}

/// Write content to file (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_get_file_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageGetFileArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::get_file::get_file);
}

pub type LoreStorageUploadArgs = crate::storage::upload::LoreStorageUploadArgs;

/// Push locally-stored, not-yet-durable content to the remote store.
///
/// Whole-call pre-dispatch fails when the handle has no remote, when `globals.offline=1`,
/// or when `globals.local=1`. Per-item: `partition == 0` → `INVALID_ARGUMENTS`; zero hash and
/// already-durable both succeed with `already_durable=1` and no remote call; missing local
/// payload → `ADDRESS_NOT_FOUND`. Otherwise the bytes are uploaded and the local entry is
/// updated with `PayloadStoredDurable` set.
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_upload(
    globals: &LoreGlobalArgs,
    args: &LoreStorageUploadArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::storage::upload::upload)
}

/// Upload deferred content (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_storage_upload_async(
    globals: &LoreGlobalArgs,
    args: &LoreStorageUploadArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::storage::upload::upload);
}

pub type LoreServiceStartArgs = crate::service::LoreServiceStartArgs;

/// Start the Lore background service.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_service_start(
    globals: &LoreGlobalArgs,
    args: &LoreServiceStartArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::service::start)
}

/// Asynchronous version of `lore_service_start`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_service_start_async(
    globals: &LoreGlobalArgs,
    args: &LoreServiceStartArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::service::start);
}

pub type LoreServiceStopArgs = crate::service::LoreServiceStopArgs;

/// Stop the Lore background service.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_service_stop(
    globals: &LoreGlobalArgs,
    args: &LoreServiceStopArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::service::stop)
}

/// Asynchronous version of `lore_service_stop`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
#[unsafe(no_mangle)]
pub extern "C" fn lore_service_stop_async(
    globals: &LoreGlobalArgs,
    args: &LoreServiceStopArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::service::stop);
}

pub type LoreNotificationSubscribeArgs = crate::notification::LoreNotificationSubscribeArgs;

/// Subscribe to repository notifications.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Notification Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_NOTIFICATION_SUBSCRIBED` | `lore_notification_subscribed_event_data_t` | Emitted when successfully subscribed to repository notifications |
/// | `LORE_EVENT_NOTIFICATION_BRANCH_CREATED` | `lore_notification_branch_created_event_data_t` | Emitted when a branch is created in the repository (push notification) |
/// | `LORE_EVENT_NOTIFICATION_BRANCH_DELETED` | `lore_notification_branch_deleted_event_data_t` | Emitted when a branch is deleted in the repository (push notification) |
/// | `LORE_EVENT_NOTIFICATION_BRANCH_PUSHED` | `lore_notification_branch_pushed_event_data_t` | Emitted when a branch is pushed to (push notification) |
/// | `LORE_EVENT_NOTIFICATION_RESOURCE_LOCKED` | `lore_notification_resource_locked_event_data_t` | Emitted when a resource is locked (push notification) |
/// | `LORE_EVENT_NOTIFICATION_RESOURCE_UNLOCKED` | `lore_notification_resource_unlocked_event_data_t` | Emitted when a resource is unlocked (push notification) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_notification_subscribe(
    globals: &LoreGlobalArgs,
    args: &LoreNotificationSubscribeArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::notification::subscribe)
}

/// Asynchronous version of `lore_notification_subscribe`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Notification Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_NOTIFICATION_SUBSCRIBED` | `lore_notification_subscribed_event_data_t` | Emitted when successfully subscribed to repository notifications |
/// | `LORE_EVENT_NOTIFICATION_BRANCH_CREATED` | `lore_notification_branch_created_event_data_t` | Emitted when a branch is created in the repository (push notification) |
/// | `LORE_EVENT_NOTIFICATION_BRANCH_DELETED` | `lore_notification_branch_deleted_event_data_t` | Emitted when a branch is deleted in the repository (push notification) |
/// | `LORE_EVENT_NOTIFICATION_BRANCH_PUSHED` | `lore_notification_branch_pushed_event_data_t` | Emitted when a branch is pushed to (push notification) |
/// | `LORE_EVENT_NOTIFICATION_RESOURCE_LOCKED` | `lore_notification_resource_locked_event_data_t` | Emitted when a resource is locked (push notification) |
/// | `LORE_EVENT_NOTIFICATION_RESOURCE_UNLOCKED` | `lore_notification_resource_unlocked_event_data_t` | Emitted when a resource is unlocked (push notification) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_notification_subscribe_async(
    globals: &LoreGlobalArgs,
    args: &LoreNotificationSubscribeArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::notification::subscribe);
}

pub type LoreNotificationUnsubscribeArgs = crate::notification::LoreNotificationUnsubscribeArgs;

/// Unsubscribe from repository notifications.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Notification Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_NOTIFICATION_UNSUBSCRIBED` | `lore_notification_unsubscribed_event_data_t` | Emitted when successfully unsubscribed from repository notifications |
#[unsafe(no_mangle)]
pub extern "C" fn lore_notification_unsubscribe(
    globals: &LoreGlobalArgs,
    args: &LoreNotificationUnsubscribeArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::notification::unsubscribe)
}

/// Asynchronous version of `lore_notification_unsubscribe`.
///
/// # Events
///
/// Events are delivered via the callback as `lore_event_t`. Use the `tag` field to identify the event type.
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_LOG` | `lore_log_event_data_t` | Diagnostic messages throughout execution |
/// | `LORE_EVENT_ERROR` | `lore_error_event_data_t` | Emitted for a non-fatal error during the operation |
/// | `LORE_EVENT_COMPLETE` | `lore_complete_event_data_t` | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | `LORE_EVENT_END` | `lore_end_event_data_t` | Always emitted after `COMPLETE` to signal callback termination |
///
/// ## Notification Events
///
/// | Tag | Data Type | Description |
/// |-----|-----------|-------------|
/// | `LORE_EVENT_NOTIFICATION_UNSUBSCRIBED` | `lore_notification_unsubscribed_event_data_t` | Emitted when successfully unsubscribed from repository notifications |
#[unsafe(no_mangle)]
pub extern "C" fn lore_notification_unsubscribe_async(
    globals: &LoreGlobalArgs,
    args: &LoreNotificationUnsubscribeArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::notification::unsubscribe);
}

/// Apply the given logging configuration.
///
/// Returns 0 when the configuration was applied and a non-zero value when it
/// was not.
#[unsafe(no_mangle)]
pub extern "C" fn lore_log_configure(config: &LoreLogConfig) -> i32 {
    log::configure(config)
}

/// Shut the library down, stopping its worker threads and releasing the
/// resources it holds. Call this once, when no further calls will be made.
///
/// Returns 0 on success and a non-zero value on failure.
#[unsafe(no_mangle)]
pub extern "C" fn lore_shutdown() -> i32 {
    crate::shutdown();
    0
}

/// Limits the total number of threads Lore sizes its pools for.
///
/// Pass `0` for "no limit", which is the default and sizes every pool from the
/// host's processor count. The `LORE_MAX_THREADS` environment variable overrides
/// this count when set above zero.
///
/// # The pools
///
/// Four pools share the limit:
///
/// - **worker** — the async runtime executing Lore's own work. Asks for one
///   thread per processor.
/// - **blocking** — OS APIs with no async form (keyring, cloud SDK
///   initialization, service IPC). Asks for 3 regardless of processor count,
///   two for the async runtime and one for the network runtime, because file
///   I/O does not run here.
/// - **net** — the network runtime carrying QUIC, TLS and HTTP/2, kept separate
///   so protocol timers are not delayed by other work. Asks for 2 on a client,
///   one per processor on a server.
/// - **io** — the syscall pool every file operation dispatches through. Asks for
///   twice the processor count, capped at 16.
///
/// # How a limit is divided
///
/// Each pool states an ideal, as above. Where a pool takes a configuration or
/// environment control (`LORE_NET_THREADS`, `LORE_IO_POOL_THREADS`, the server's
/// thread settings), that control raises the pool's ideal — it never sets the
/// pool's final size, so no knob can lift the process above a limit set here.
/// Retired per-pool variables are ignored outright.
///
/// With no limit, every pool gets its ideal. With one, the pools are scaled to
/// fit by the largest-remainder method: each is granted its proportional share
/// of the limit, and the threads left over by rounding go to the pools with the
/// largest remainders. A pool never drops below 2 threads, since a starved pool
/// can deadlock work another pool waits on. That floor outranks the limit, so a
/// limit below 8 yields 8.
///
/// # What is not counted
///
/// Threads the host application or a linked library creates. Lore accounts only
/// for its own.
///
/// Must be called before the first Lore operation, while the runtime is still
/// unconstructed. Returns `0` if the limit was applied, `1` if it had already
/// been set (or the runtime was already running).
#[unsafe(no_mangle)]
pub extern "C" fn lore_set_thread_limit(count: usize) -> i32 {
    if crate::set_thread_limit(count) { 0 } else { 1 }
}

pub type LoreAllocFn = unsafe extern "C" fn(align: usize, size: usize) -> *mut std::ffi::c_void;
pub type LoreAllocZeroedFn =
    unsafe extern "C" fn(align: usize, size: usize) -> *mut std::ffi::c_void;
pub type LoreReallocFn = unsafe extern "C" fn(
    ptr: *mut std::ffi::c_void,
    align: usize,
    size: usize,
) -> *mut std::ffi::c_void;
pub type LoreDeallocFn = unsafe extern "C" fn(ptr: *mut std::ffi::c_void);

/// Install the memory allocator the library uses for its own allocations.
/// Provide functions for allocation, zeroed allocation, reallocation and
/// freeing. Call this before the library makes its first allocation; once it
/// has allocated, the allocator can no longer be changed.
///
/// Returns 0 when the allocator was installed and a non-zero value when it was
/// too late to install one, in which case the call does nothing.
#[unsafe(no_mangle)]
pub extern "C" fn lore_set_allocator(
    alloc: LoreAllocFn,
    alloc_zeroed: LoreAllocZeroedFn,
    realloc: LoreReallocFn,
    dealloc: LoreDeallocFn,
) -> i32 {
    if lore_base::allocator::set_external_allocator(lore_base::allocator::ExternalAllocator {
        alloc,
        alloc_zeroed,
        realloc,
        dealloc,
    }) {
        0
    } else {
        1
    }
}

/// Return the library version as a NUL-terminated string. The string is owned
/// by the library and must not be freed by the caller.
#[unsafe(no_mangle)]
pub extern "C" fn lore_version() -> *const std::ffi::c_char {
    lore_base::version::LORE_LIBRARY_VERSION_CSTR
        .as_ptr()
        .cast::<std::ffi::c_char>()
}

pub fn user_directory() -> Option<PathBuf> {
    lore_base::directories::project_directory().map(|path| path.config_local_dir().to_path_buf())
}

/// Return the path of the directory where the library keeps its per-user data
/// as a NUL-terminated string. The string is owned by the library and must not
/// be freed by the caller.
#[unsafe(no_mangle)]
pub extern "C" fn lore_user_directory() -> *const std::ffi::c_char {
    static CAPI_USER_DIRECTORY: LazyLock<CString> = LazyLock::new(|| {
        user_directory()
            .map(|path| CString::new(path.display().to_string()).unwrap_or_default())
            .unwrap_or_default()
    });

    CAPI_USER_DIRECTORY.as_c_str().as_ptr()
}

pub type LoreRepositoryMetadataGetArgs = crate::repository::LoreRepositoryMetadataGetArgs;

/// Retrieve repository metadata. Reads a single key, or all entries when no
/// key is given.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_metadata_get(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::metadata_get)
}

/// Asynchronous version of `lore_repository_metadata_get`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_metadata_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::metadata_get);
}

pub type LoreRepositoryMetadataSetArgs = crate::repository::LoreRepositoryMetadataSetArgs;

/// Set repository metadata key-value pairs.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_metadata_set(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::metadata_set)
}

/// Asynchronous version of `lore_repository_metadata_set`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_metadata_set_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::metadata_set);
}

pub type LoreRepositoryMetadataClearArgs = crate::repository::LoreRepositoryMetadataClearArgs;

/// Clear repository metadata keys. Clears all user-defined keys when none are
/// given.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_metadata_clear(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::metadata_clear)
}

/// Asynchronous version of `lore_repository_metadata_clear`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_metadata_clear_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::metadata_clear);
}

pub type LoreRepositoryInstanceListArgs = crate::repository::LoreRepositoryInstanceListArgs;

/// List the tracked instances of the repository.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_instance_list(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryInstanceListArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::instance_list)
}

/// Asynchronous version of `lore_repository_instance_list`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_instance_list_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryInstanceListArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::instance_list);
}

pub type LoreRepositoryInstancePruneArgs = crate::repository::LoreRepositoryInstancePruneArgs;

/// Remove stale instances of the repository that are no longer present.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_instance_prune(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryInstancePruneArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::instance_prune)
}

/// Asynchronous version of `lore_repository_instance_prune`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_instance_prune_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryInstancePruneArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::instance_prune);
}

pub type LoreRepositoryUpdatePathArgs = crate::repository::LoreRepositoryUpdatePathArgs;

/// Update the recorded path of the current repository instance to its present
/// location.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_update_path(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryUpdatePathArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::repository::repository_update_path,
    )
}

/// Asynchronous version of `lore_repository_update_path`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_update_path_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryUpdatePathArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::repository::repository_update_path,
    );
}

pub type LoreRepositoryConfigGetArgs = crate::repository::LoreRepositoryConfigGetArgs;

/// Read a configuration value of the current repository by key.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_config_get(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryConfigGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::repository::config_get)
}

/// Asynchronous version of `lore_repository_config_get`.
#[unsafe(no_mangle)]
pub extern "C" fn lore_repository_config_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreRepositoryConfigGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::repository::config_get);
}

pub type LoreRevisionTreeLoadArgs = crate::revision_tree::load::LoreRevisionTreeLoadArgs;

/// Open a memory-based revision tree handle on the given
/// `(store, repository, revision_hash)` tuple. `revision_hash == 0` opens an
/// empty tree suitable for committing an initial revision.
///
/// | Terminal event                       | Payload                                | Notes                                              |
/// |--------------------------------------|----------------------------------------|----------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_LOADED`    | `lore_revision_tree_loaded_event_data_t` | Emitted on success carrying the opened handle id |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_load(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeLoadArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision_tree::load::load)
}

/// Open a memory-based revision tree handle (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_load_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeLoadArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision_tree::load::load);
}

pub type LoreRevisionTreeCloseArgs = crate::revision_tree::close::LoreRevisionTreeCloseArgs;

/// Release a memory-based revision tree handle.
///
/// Subsequent calls against the same handle return `InvalidArguments`. The
/// call blocks until every in-flight op on the handle has paired its
/// decrement.
///
/// | Terminal event                              | Payload                                       | Notes                                              |
/// |---------------------------------------------|-----------------------------------------------|----------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_CLOSE_COMPLETE`   | `lore_revision_tree_close_complete_event_data_t` | Emitted on success carrying the caller id       |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_close(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeCloseArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision_tree::close::close)
}

/// Release a memory-based revision tree handle (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_close_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeCloseArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision_tree::close::close);
}

pub type LoreRevisionTreeResolvePathArgs =
    crate::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs;

/// Resolve a UTF-8 path against a loaded revision tree to a node id. An empty
/// path resolves to the root node.
///
/// | Terminal event                                       | Payload                                             | Notes                                                       |
/// |------------------------------------------------------|-----------------------------------------------------|-------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_RESOLVE_PATH_COMPLETE`     | `lore_revision_tree_resolve_path_complete_event_data_t` | Carries the resolved node id and the per-call outcome   |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_resolve_path(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeResolvePathArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::resolve_path::resolve_path,
    )
}

/// Resolve a UTF-8 path against a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_resolve_path_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeResolvePathArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::resolve_path::resolve_path,
    );
}

pub type LoreRevisionTreeListChildrenArgs =
    crate::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;

/// Stream the children of a directory node in a loaded revision tree.
///
/// | Terminal event                       | Payload                                | Notes                                                          |
/// |--------------------------------------|----------------------------------------|----------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_CHILD`     | `lore_revision_tree_child_event_data_t` | One per child; an empty directory emits none before `Complete` |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_list_children(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeListChildrenArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::list_children::list_children,
    )
}

/// Stream the children of a directory node (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_list_children_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeListChildrenArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::list_children::list_children,
    );
}

pub type LoreRevisionTreeNodeInfoArgs =
    crate::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;

/// Fetch the per-node record for a single node id in a loaded revision tree.
///
/// | Terminal event                          | Payload                                     | Notes                                                          |
/// |-----------------------------------------|---------------------------------------------|----------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_NODE_INFO`    | `lore_revision_tree_node_info_event_data_t` | Carries the node record, uniform across every node id (revision metadata: `lore_revision_tree_info`) |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_node_info(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeNodeInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::node_info::node_info,
    )
}

/// Fetch the per-node record for a single node id (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_node_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeNodeInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::node_info::node_info,
    );
}

pub type LoreRevisionTreeInfoArgs = crate::revision_tree::info::LoreRevisionTreeInfoArgs;

/// Fetch the loaded revision's record-level metadata (parents, creation
/// timestamp, author identity, metadata key count). Revision-scoped — no node id.
///
/// | Terminal event                     | Payload                                | Notes                                                   |
/// |------------------------------------|----------------------------------------|---------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_INFO`    | `lore_revision_tree_info_event_data_t` | Carries the revision record metadata for the handle     |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_info(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeInfoArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision_tree::info::info)
}

/// Fetch the loaded revision's record-level metadata (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_info_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeInfoArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision_tree::info::info);
}

pub type LoreRevisionTreeNodePathArgs =
    crate::revision_tree::node_path::LoreRevisionTreeNodePathArgs;

/// Reconstruct the full UTF-8 path for a node id by walking parent pointers,
/// relative to the handle's own tree root.
///
/// | Terminal event                       | Payload                                     | Notes                                                  |
/// |--------------------------------------|---------------------------------------------|--------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_NODE_PATH` | `lore_revision_tree_node_path_event_data_t` | Carries the path; the root resolves to the empty path  |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_node_path(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeNodePathArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::node_path::node_path,
    )
}

/// Reconstruct the full UTF-8 path for a node id (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_node_path_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeNodePathArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::node_path::node_path,
    );
}

pub type LoreRevisionTreeAddArgs = crate::revision_tree::add::LoreRevisionTreeAddArgs;

/// Add a batch of nodes to a loaded revision tree. An entry parents onto an
/// existing node or onto an earlier entry, so one call builds a subtree. Every
/// entry is checked before any node is created, so one bad entry rejects the
/// call and creates nothing; the reason names the offending entry's batch index,
/// which a caller leaving `entry_id` at zero has no other way to identify. A failure
/// after those checks pass is internal and may leave part of the batch created.
///
/// A link entry's target revision is not resolved here, so a link naming a
/// revision that cannot be read is accepted and fails only when something later
/// reads through it. Entries under separate parents are created concurrently,
/// but allocating a node slot is serialized per loaded tree.
///
/// | Terminal event                            | Payload                                          | Notes                                                    |
/// |-------------------------------------------|--------------------------------------------------|----------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_ADD_COMPLETE`   | `lore_revision_tree_add_complete_event_data_t`   | One per entry, carrying its `entry_id`                    |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE` | `lore_revision_tree_batch_complete_event_data_t` | Exactly one, carrying the `batch_id` and the call's outcome |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_add(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeAddArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(globals, args, callback, crate::revision_tree::add::add)
}

/// Add a batch of nodes to a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_add_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeAddArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(globals, args, callback, crate::revision_tree::add::add);
}

pub type LoreRevisionTreeDeleteArgs = crate::revision_tree::delete::LoreRevisionTreeDeleteArgs;

/// Remove a batch of subtrees from a loaded revision tree. Each entry names a
/// subtree root and removes it whole, transitive children included. Every entry
/// is checked before any node is touched, so one bad entry rejects the call and
/// leaves every subtree in place; the reason names the offending entry's batch
/// index, which a caller leaving `entry_id` at zero has no other way to
/// identify. A failure after those checks pass is internal and may leave part of
/// the batch removed.
///
/// A node the loaded revision holds is **staged** for deletion and stays in the
/// tree: it keeps its name and its place among its siblings, still lists through
/// `lore_revision_tree_list_children`, and reports
/// `LORE_NODE_STAGED_ACTION_DELETE` in its `staged_action`. The commit that
/// freezes the tree is what drops it. A node added through this handle is in no
/// revision yet, so it is discarded outright instead, freeing its name and its
/// node id. A link is removed as one node — its subtree belongs to the linked
/// repository's tree.
///
/// A staged deletion is reversible: `lore_revision_tree_add` of the same name
/// under the same parent with the same kind restores the node. A zero
/// `address.context` on that add preserves the node's `file_id`; supplying one
/// replaces it, as on `lore_revision_tree_modify`. Only the named node comes back
/// — restoring a directory leaves its children staged for deletion, so each has to
/// be added back in turn. A discarded node is not restorable, since its id is
/// gone.
///
/// Staging fans out one depth level at a time; discarding an added node rewrites
/// sibling pointers and so runs serially, deepest first. Memory while the call
/// runs is proportional to the widest level of the subtrees being removed rather
/// than to the entry count, since a level is collected before it is staged. The
/// root cannot be deleted, and an entry whose ancestor another entry deletes is
/// rejected rather than removed twice.
///
/// | Terminal event                              | Payload                                           | Notes                                                    |
/// |---------------------------------------------|---------------------------------------------------|----------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_DELETE_COMPLETE`  | `lore_revision_tree_delete_complete_event_data_t` | One per entry, carrying its `entry_id` and `node_count`   |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE`   | `lore_revision_tree_batch_complete_event_data_t`  | Exactly one, carrying the `batch_id` and the call's outcome |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_delete(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::delete::delete,
    )
}

/// Remove a batch of subtrees from a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_delete_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::delete::delete,
    );
}

pub type LoreRevisionTreeModifyArgs = crate::revision_tree::modify::LoreRevisionTreeModifyArgs;

/// Rewrite a batch of file nodes' `mode`, `size` and `address` in a loaded
/// revision tree. Every entry is checked before any node is rewritten, so one bad
/// entry rejects the call and leaves every target untouched; the reason names the
/// offending entry's batch index, which a caller leaving `entry_id` at zero has no
/// other way to identify. A failure after those checks pass is internal and may
/// leave part of the batch rewritten.
///
/// Only a file is modifiable: a directory's size and address are derived at
/// commit and a link's address is its target. A zero `address.context` preserves
/// the node's existing file id rather than generating one, which is the opposite
/// of `lore_revision_tree_add` — the node already has an identity, and replacing
/// it would record the edit as a move. Entries touch no parent or sibling chain,
/// so the whole batch applies concurrently.
///
/// | Terminal event                              | Payload                                           | Notes                                                    |
/// |---------------------------------------------|---------------------------------------------------|----------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_MODIFY_COMPLETE`  | `lore_revision_tree_modify_complete_event_data_t` | One per entry, carrying its `entry_id`                    |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE`   | `lore_revision_tree_batch_complete_event_data_t`  | Exactly one, carrying the `batch_id` and the call's outcome |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_modify(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeModifyArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::modify::modify,
    )
}

/// Rewrite a batch of file nodes in a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_modify_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeModifyArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::modify::modify,
    );
}

pub type LoreRevisionTreeMoveArgs = crate::revision_tree::move_node::LoreRevisionTreeMoveArgs;

/// Move a batch of nodes to new parents and/or new names in a loaded revision tree. An
/// entry naming the node's current parent renames it where it is. Every entry is checked
/// before any node is moved, so one bad entry rejects the call and leaves every node
/// where it was; the reason names the offending entry's batch index, which a caller
/// leaving `entry_id` at zero has no other way to identify. A failure after those checks
/// pass is internal and may leave earlier entries applied.
///
/// A move keeps the node: its node id, its `file_id` and its children come along, and
/// the change is recorded as a move rather than as a deletion and an addition, so the
/// revision graph carries the node's history across it. The node reports
/// `LORE_NODE_STAGED_ACTION_MOVE` until the commit that freezes the tree, and so does
/// every node under a moved directory — their records do not change, but their paths do.
/// Two exceptions: a node added through this handle stays staged as an addition wherever
/// it lands, since it is in no revision a move could be recorded against; and a node
/// under the moved directory that is staged for deletion keeps its deletion, since it is
/// leaving the revision at the commit either way.
///
/// Both batch-level rules read the tree the whole batch produces rather than the one in
/// front of them. A destination inside the moved node's own subtree is rejected, and so
/// is one that lands there once the batch is applied — moving A under B and B under A is
/// a loop neither entry shows on its own. A name a live child of the destination already
/// holds is rejected, but a name the batch itself vacates is not: moving `x` out of a
/// directory while moving another node to `x` in it succeeds, and two entries taking one
/// name under one destination reject even though neither collides with the tree.
///
/// Entries apply one at a time, in batch order, because a move rewrites the parent and
/// sibling pointers of two child chains where `lore_revision_tree_add` only prepends to
/// one. For the same reason concurrent calls have more to lose here than on `add` or
/// `modify`: two calls moving nodes that share a parent chain can interleave their
/// unlinks, which the pre-commit validator then refuses. Moves that may touch one parent
/// chain belong in one call.
///
/// | Terminal event                            | Payload                                          | Notes                                                       |
/// |-------------------------------------------|--------------------------------------------------|-------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_MOVE_COMPLETE`  | `lore_revision_tree_move_complete_event_data_t`  | One per entry, carrying its `entry_id` and the moved node    |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE` | `lore_revision_tree_batch_complete_event_data_t` | Exactly one, carrying the `batch_id` and the call's outcome  |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_move(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMoveArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::move_node::move_node,
    )
}

/// Move a batch of nodes in a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_move_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMoveArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::move_node::move_node,
    );
}

pub type LoreRevisionTreeMetadataSetArgs =
    crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;

/// Record a batch of `(key, value)` pairs on a loaded revision tree's in-progress
/// metadata.
///
/// `value` is a `lore_metadata_t`, which carries its own kind — set the union's
/// `tag` and the matching member. There is no separate format field and nothing
/// is parsed, so a value cannot be stored under a kind it is not, and a binary
/// value can hold any bytes rather than only text. This differs from
/// `lore_revision_metadata_set`, which takes text plus a parallel format array.
/// `lore_revision_tree_metadata_get` returns the same union.
///
/// Every entry is checked before any pair is recorded, so one bad entry rejects
/// the call and records nothing; the reason names the offending entry's batch
/// index, which a caller leaving `entry_id` at zero has no other way to
/// identify.
///
/// A repeated key is **not** rejected, unlike the duplicate-target rules on the
/// node verbs: entries apply in index order, so the last entry naming a key wins
/// — the same result as sending those pairs as separate calls.
///
/// Nothing reaches storage here. The pairs live on the handle until
/// `lore_revision_tree_commit` serializes them, so only this handle's
/// `lore_revision_tree_metadata_get` sees them. The whole batch applies under one
/// write lock, which is what makes it atomic and why there is no concurrency to
/// gain: the work is buffer writes, not I/O.
///
/// A revision's whole metadata is capped at 1 MiB. The cap counts the metadata
/// itself — keys, values and per-entry overhead — and not what a value refers
/// to: a value holding a content address costs the address, not the content
/// behind it.
///
/// A single entry larger than the whole cap is rejected here, since no amount of
/// removing other keys could make it fit. The running total is not checked here,
/// because what a revision ends up carrying is only known once every set has
/// run: a batch of individually legal entries that together push past the limit
/// is reported as recorded and fails later, at `lore_revision_tree_commit`.
///
/// | Terminal event                                    | Payload                                                 | Notes                                                       |
/// |---------------------------------------------------|---------------------------------------------------------|-------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_METADATA_SET_COMPLETE`  | `lore_revision_tree_metadata_set_complete_event_data_t` | One per entry, carrying its `entry_id`                      |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE`         | `lore_revision_tree_batch_complete_event_data_t`        | Exactly one, carrying the `batch_id` and the call's outcome  |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_metadata_set(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::metadata_set::metadata_set,
    )
}

/// Record a batch of metadata pairs on a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_metadata_set_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMetadataSetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::metadata_set::metadata_set,
    );
}

pub type LoreRevisionTreeMetadataGetArgs =
    crate::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;

/// Read a batch of metadata values from a loaded revision tree.
///
/// By default this reads only the **revision being built** — what
/// `lore_revision_tree_metadata_set` recorded on this handle, which is exactly
/// what `lore_revision_tree_commit` will write. Nothing is inherited from the
/// revision the handle was loaded on. Set `include_revision` to `1` to also fall
/// back to that revision for a key the handle has no entry for; a pending entry
/// still wins, so the flag only adds answers.
///
/// A key present in neither emits **no event at all** and does not fail the call,
/// matching `lore_revision_metadata_get`: detect an absent key by tracking
/// whether a value event arrived for its `entry_id`. This verb is therefore **not
/// all-or-nothing**, unlike the other batch verbs — it mutates nothing, so one
/// unanswerable key costs the others nothing. Bad arguments still reject the
/// whole call.
///
/// A value comes back as the same `lore_metadata_t` that
/// `lore_revision_tree_metadata_set` takes, so it round-trips without either
/// side encoding it as text. Every kind is returned, raw binary included.
///
/// A value whose stored bytes do not match the kind they are tagged with, or
/// whose tag this build does not recognize, reports `LORE_ERROR_CODE_INTERNAL`
/// on that entry rather than staying silent, so it is never mistaken for an
/// absent key.
///
/// The revision's metadata is read once for the whole batch, which is what
/// batching buys here.
///
/// | Terminal event                                    | Payload                                                 | Notes                                                       |
/// |---------------------------------------------------|---------------------------------------------------------|-------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_METADATA_GET_COMPLETE`  | `lore_revision_tree_metadata_get_complete_event_data_t` | One per key that resolved, carrying its `entry_id`; none for an absent key |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE`         | `lore_revision_tree_batch_complete_event_data_t`        | Exactly one, carrying the `batch_id` and the call's outcome  |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_metadata_get(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::metadata_get::metadata_get,
    )
}

/// Read a batch of metadata values from a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_metadata_get_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMetadataGetArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::metadata_get::metadata_get,
    );
}

pub type LoreRevisionTreeMetadataClearArgs =
    crate::revision_tree::metadata_clear::LoreRevisionTreeMetadataClearArgs;

/// Remove a batch of keys from a loaded revision tree's in-progress metadata.
/// Every entry is checked before any key is removed, so one bad entry rejects the
/// call and removes nothing; the reason names the offending entry's batch index,
/// which a caller leaving `entry_id` at zero has no other way to identify.
///
/// **Clearing a key that is not set is a no-op success**, not a failure. The
/// terminal's `removed` field says which happened: `1` when the key was there and
/// is now gone, `0` when there was nothing to remove. A repeated key is likewise
/// not rejected — the second entry naming it reports `removed = 0`.
///
/// This clears the **pending** metadata that `lore_revision_tree_metadata_set`
/// records and `lore_revision_tree_metadata_get` reads first; a key frozen in the
/// loaded revision is not reachable from here, since clearing edits the revision
/// being built and the one it was loaded from is immutable. The whole batch
/// applies under one write lock, which is what makes it atomic.
///
/// | Terminal event                                      | Payload                                                   | Notes                                                       |
/// |-----------------------------------------------------|-----------------------------------------------------------|-------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_METADATA_CLEAR_COMPLETE`  | `lore_revision_tree_metadata_clear_complete_event_data_t` | One per entry, carrying its `entry_id` and `removed`        |
/// | `LORE_EVENT_REVISION_TREE_BATCH_COMPLETE`           | `lore_revision_tree_batch_complete_event_data_t`          | Exactly one, carrying the `batch_id` and the call's outcome  |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_metadata_clear(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::metadata_clear::metadata_clear,
    )
}

/// Remove a batch of metadata keys from a loaded revision tree (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_metadata_clear_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeMetadataClearArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::metadata_clear::metadata_clear,
    );
}

pub type LoreRevisionTreeCommitArgs = crate::revision_tree::commit::LoreRevisionTreeCommitArgs;
pub type LoreRevisionTreeCommitOptions =
    crate::revision_tree::commit::LoreRevisionTreeCommitOptions;

/// Freeze a loaded revision tree into a new revision and advance its branch tip.
///
/// **The branch is not an argument.** It is the revision's own, read from the
/// `branch` metadata key: set it with `lore_revision_tree_metadata_set` to start a
/// branch's history, and leave it unset to continue the loaded revision's branch. A
/// key that is set must name either the loaded revision's branch or a branch whose
/// branch point is exactly the loaded revision. A handle loaded from the zero
/// revision has no parent to read a branch from and must set the key.
///
/// The revision records exactly the metadata set on the handle — nothing is
/// inherited from the revision it was loaded on — plus the three facts about the
/// commit the caller did not supply: the branch, the timestamp if unset, and
/// `created-by` / `committed-by` if unset. The commit message is caller metadata like
/// any other: set `"message"` before committing.
///
/// On success the handle stays usable and now *is* the new revision: node ids
/// captured before the commit still resolve, and further edits commit on top. The
/// pending metadata is emptied, so the next revision starts fresh.
///
/// **A commit is all-or-nothing against the handle.** Either it succeeds and the
/// handle is consistent on the new revision, or it fails and the handle is
/// consistent on the state it had before the call. A call rejected before any write
/// — nothing staged, an unusable branch, a tree the validator refuses, or a branch
/// tip that has already moved — writes nothing at all. A failure once the freeze has
/// begun leaves a part-frozen tree, which is discarded and rebuilt from a snapshot
/// taken before the freeze started, so the handle comes back on the revision it was
/// on with the edits still staged. Either way recovery is to fix what the terminal
/// reported and retry **on the same handle**: no close, no reload, no re-applying
/// edits. The one failure that still poisons the handle is a restore that itself
/// fails, which reports `INTERNAL` saying so. When the branch had advanced,
/// `new_tip_hash` on the terminal carries that tip, which is also how a caller tells
/// that failure apart: neither a tip collision nor an empty commit has a
/// `lore_error_code_t` of its own, so both report `INTERNAL` with the reason in the
/// completion detail — the same codes the file-system commit returns.
///
/// `options.remote_write = 1` uploads within the call. It is a request, not a
/// guarantee: a store bound offline or local-only, or a call passing
/// `globals.local`, silently commits local-only. So does a store opened without a
/// remote configuration — there is nothing to upload to, and the commit still
/// reports success. Per-call flags contradicting the store's bound flags reject the
/// call.
///
/// **The commit holds the handle for the length of the call.** No other call on the
/// same handle runs while the tree is frozen, so an edit issued concurrently lands
/// wholly before the commit reads the tree or wholly after it finishes, two commits
/// on one handle serialize, and `metadata_set` can no longer lose an edit to the
/// commit. A commit on a large tree therefore blocks reads on that handle for its
/// duration, and a callback that re-enters the API on the same handle deadlocks —
/// which the callback contract already forbids. Commits from *different* handles or
/// processes still race, and the branch tip compare-and-swap decides them.
///
/// `remote_write` is resolved onto the handle's shared repository context, so the
/// value outlives the call: the handle carries whatever the last commit resolved.
///
/// | Terminal event                                | Payload                                             | Notes                                                             |
/// |-----------------------------------------------|-----------------------------------------------------|-------------------------------------------------------------------|
/// | `LORE_EVENT_REVISION_TREE_COMMIT_COMPLETE`    | `lore_revision_tree_commit_complete_event_data_t`   | Exactly one; carries the new revision, or the new tip on collision |
/// | `LORE_EVENT_REVISION_COMMIT_REVISION`         | `lore_revision_commit_revision_event_data_t`        | On success, for continuity with file-system commit consumers       |
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_commit(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeCommitArgs,
    callback: LoreEventCallbackConfig,
) -> i32 {
    run_synchronously(
        globals,
        args,
        callback,
        crate::revision_tree::commit::commit,
    )
}

/// Freeze a loaded revision tree into a new revision (async variant).
#[unsafe(no_mangle)]
pub extern "C" fn lore_revision_tree_commit_async(
    globals: &LoreGlobalArgs,
    args: &LoreRevisionTreeCommitArgs,
    callback: LoreEventCallbackConfig,
) {
    run_asynchronously(
        globals,
        args,
        callback,
        crate::revision_tree::commit::commit,
    );
}
