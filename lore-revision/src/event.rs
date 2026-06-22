// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(non_camel_case_types)]
#![allow(unused_parens)]

pub mod metadata;
pub mod revision_tree;

use lore_macro::VariantTypeSize;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::LoreAuthUrlEventData;
use crate::auth::userinfo::LoreAuthIdentityEventData;
use crate::auth::userinfo::LoreAuthUserInfoEventData;
use crate::auth::userinfo::LoreAuthUserTokenEventData;
use crate::branch::LoreBranchArchiveEventData;
use crate::branch::LoreBranchCreateEventData;
use crate::branch::LoreBranchDiffBeginEventData;
use crate::branch::LoreBranchDiffChangeBeginEventData;
use crate::branch::LoreBranchDiffChangeEndEventData;
use crate::branch::LoreBranchDiffChangeEventData;
use crate::branch::LoreBranchDiffConflictBeginEventData;
use crate::branch::LoreBranchDiffConflictEndEventData;
use crate::branch::LoreBranchDiffConflictEventData;
use crate::branch::LoreBranchDiffEndEventData;
use crate::branch::LoreBranchListBeginEventData;
use crate::branch::LoreBranchListEndEventData;
use crate::branch::LoreBranchListEntryEventData;
use crate::branch::LoreBranchProtectEventData;
use crate::branch::LoreBranchUnprotectEventData;
use crate::branch::info::LoreBranchInfoEventData;
use crate::branch::latest::LoreBranchLatestListEntryEventData;
use crate::branch::merge::LoreBranchMergeAbortBeginEventData;
use crate::branch::merge::LoreBranchMergeAbortEndEventData;
use crate::branch::merge::LoreBranchMergeConflictFileEventData;
use crate::branch::merge::LoreBranchMergeIntoFileBeginEventData;
use crate::branch::merge::LoreBranchMergeIntoFileEndEventData;
use crate::branch::merge::LoreBranchMergeIntoFileEventData;
use crate::branch::merge::LoreBranchMergeIntoFragmentBeginEventData;
use crate::branch::merge::LoreBranchMergeIntoFragmentEndEventData;
use crate::branch::merge::LoreBranchMergeIntoFragmentProgressEventData;
use crate::branch::merge::LoreBranchMergeIntoRevisionEventData;
use crate::branch::merge::LoreBranchMergeIntoSyncBeginEventData;
use crate::branch::merge::LoreBranchMergeIntoSyncEndEventData;
use crate::branch::merge::LoreBranchMergeResolveFileEventData;
use crate::branch::merge::LoreBranchMergeResolveRevisionEventData;
use crate::branch::merge::LoreBranchMergeStartBeginEventData;
use crate::branch::merge::LoreBranchMergeStartEndEventData;
use crate::branch::merge::LoreBranchMergeUnresolveFileEventData;
use crate::branch::merge::LoreBranchMergeUnresolveRevisionEventData;
use crate::branch::push::LoreBranchPushBranchCreateBeginEventData;
use crate::branch::push::LoreBranchPushBranchCreateEndEventData;
use crate::branch::push::LoreBranchPushEventData;
use crate::branch::push::LoreBranchPushFragmentBeginEventData;
use crate::branch::push::LoreBranchPushFragmentEndEventData;
use crate::branch::push::LoreBranchPushFragmentProgressEventData;
use crate::branch::push::LoreBranchPushRevisionPushBeginEventData;
use crate::branch::push::LoreBranchPushRevisionPushEndEventData;
use crate::branch::push::LoreBranchPushRevisionPushUpdateEventData;
use crate::branch::push::LoreBranchPushRevisionUpdateBeginEventData;
use crate::branch::push::LoreBranchPushRevisionUpdateEndEventData;
use crate::branch::reset::LoreBranchResetEventData;
use crate::commit::LoreRevisionCommitBeginEventData;
use crate::commit::LoreRevisionCommitEndEventData;
use crate::commit::LoreRevisionCommitProgressEventData;
use crate::commit::LoreRevisionCommitRevisionEventData;
use crate::dependency::LoreDependencyResolveBeginEventData;
use crate::dependency::LoreDependencyResolveEndEventData;
use crate::dependency::LoreDependencyResolveItemEventData;
use crate::dependency::LoreFileDependencyAddBeginEventData;
use crate::dependency::LoreFileDependencyAddEndEventData;
use crate::dependency::LoreFileDependencyAddEntryEventData;
use crate::dependency::LoreFileDependencyListBeginEventData;
use crate::dependency::LoreFileDependencyListEndEventData;
use crate::dependency::LoreFileDependencyListEntryEventData;
use crate::dependency::LoreFileDependencyListFileEndEventData;
use crate::dependency::LoreFileDependencyListFileEventData;
use crate::dependency::LoreFileDependencyRemoveBeginEventData;
use crate::dependency::LoreFileDependencyRemoveEndEventData;
use crate::dependency::LoreFileDependencyRemoveEntryEventData;
use crate::event::revision_tree::LoreRevisionTreeAddCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeChildEventData;
use crate::event::revision_tree::LoreRevisionTreeCloseCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeCommitCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeDeleteCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeLoadedEventData;
use crate::event::revision_tree::LoreRevisionTreeMetadataGetCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeMetadataSetCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeModifyCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeMoveCompleteEventData;
use crate::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use crate::event::revision_tree::LoreRevisionTreeNodePathEventData;
use crate::event::revision_tree::LoreRevisionTreeResolvePathCompleteEventData;
use crate::file::diff::LoreFileDiffEventData;
use crate::file::dump::LoreFileDumpEventData;
use crate::file::hash::LoreFileHashEventData;
use crate::file::history::LoreFileHistoryEventData;
use crate::file::info::LoreFileInfoEventData;
use crate::file::obliterate::LoreFileObliterateEventData;
use crate::file::reset::LoreFileResetBeginEventData;
use crate::file::reset::LoreFileResetEndEventData;
use crate::file::reset::LoreFileResetFileEventData;
use crate::file::reset::LoreFileResetProgressEventData;
use crate::file::unstage::LoreFileUnstageBeginEventData;
use crate::file::unstage::LoreFileUnstageEndEventData;
use crate::file::unstage::LoreFileUnstageFileEventData;
use crate::file::unstage::LoreFileUnstageProgressEventData;
use crate::file::unstage::LoreFileUnstageRevisionEventData;
use crate::file::write::LoreFileWriteEventData;
use crate::filter::LoreFilterExcludeEventData;
use crate::find::LoreRevisionFindEventData;
use crate::immutable::LoreFragmentWriteEventData;
use crate::instance::LoreBranchMultipleInstanceEventData;
use crate::instance::LoreRepositoryInstanceEventData;
use crate::interface::LoreError;
use crate::interface::LoreEventCallback;
use crate::interface::LoreEventCallbackConfig;
use crate::interface::LoreMetadata;
use crate::interface::LoreString;
use crate::layer::LoreLayerAddEventData;
use crate::layer::LoreLayerEntryEventData;
use crate::layer::LoreLayerRemoveEventData;
use crate::layer::LoreLayerStagedEntryEventData;
use crate::link::LoreLinkChangeEventData;
use crate::link::LoreLinkEntryEventData;
use crate::link::list::LoreLinkStagedEntryEventData;
use crate::lock::file::acquire::LoreLockFileAcquireEventData;
use crate::lock::file::acquire::LoreLockFileAcquireIgnoreEventData;
use crate::lock::file::query::LoreLockFileQueryBeginEventData;
use crate::lock::file::query::LoreLockFileQueryEventData;
use crate::lock::file::release::LoreLockFileReleaseEventData;
use crate::lock::file::release::LoreLockFileReleaseNotFoundEventData;
use crate::lock::file::status::LoreLockFileStatusBeginEventData;
use crate::lock::file::status::LoreLockFileStatusEventData;
use crate::lore::execution_context;
use crate::metadata::Metadata;
use crate::metadata::MetadataError;
use crate::metadata::MetadataType;
use crate::metadata::clear::LoreMetadataClearFileEventData;
use crate::metadata::clear::LoreMetadataClearRevisionEventData;
use crate::notification::LoreNotificationBranchCreatedEventData;
use crate::notification::LoreNotificationBranchDeletedEventData;
use crate::notification::LoreNotificationBranchPushedEventData;
use crate::notification::LoreNotificationResourceLockedEventData;
use crate::notification::LoreNotificationResourceUnlockedEventData;
use crate::notification::LoreNotificationSubscribedEventData;
use crate::notification::LoreNotificationUnsubscribedEventData;
use crate::path::LorePathIgnoreEventData;
use crate::repository::LoreBranchSwitchBeginEventData;
use crate::repository::LoreBranchSwitchEndEventData;
use crate::repository::LoreRepositoryConfigGetEventData;
use crate::repository::LoreRepositoryDumpBeginEventData;
use crate::repository::LoreRepositoryDumpEndEventData;
use crate::repository::clone::LoreRepositoryCloneBeginEventData;
use crate::repository::clone::LoreRepositoryCloneEndEventData;
use crate::repository::clone::LoreRepositoryCloneProgressEventData;
use crate::repository::create::LoreRepositoryCreateEventData;
use crate::repository::info::LoreRepositoryDataEventData;
use crate::repository::list::LoreRepositoryListEntryEventData;
use crate::repository::status::LoreRepositoryStatusCountEventData;
use crate::repository::status::LoreRepositoryStatusFileEventData;
use crate::repository::status::LoreRepositoryStatusRevisionEventData;
use crate::repository::status::LoreRepositoryStatusSummaryEventData;
use crate::repository::store::LoreRepositoryStoreImmutableQueryEventData;
use crate::repository::verify::LoreRepositoryVerifyFragmentEventData;
use crate::repository::verify::LoreRepositoryVerifyFragmentMatchEventData;
use crate::repository::verify::LoreRepositoryVerifyFragmentRemoteEventData;
use crate::repository::verify::LoreRepositoryVerifyStateBeginEventData;
use crate::repository::verify::LoreRepositoryVerifyStateEndEventData;
use crate::revision::LoreRevisionResolveEventData;
use crate::revision::bisect::LoreRevisionBisectEventData;
use crate::revision::cherry_pick::LoreCherryPickAbortBeginEventData;
use crate::revision::cherry_pick::LoreCherryPickAbortEndEventData;
use crate::revision::cherry_pick::LoreCherryPickConflictFileEventData;
use crate::revision::cherry_pick::LoreCherryPickResolveFileEventData;
use crate::revision::cherry_pick::LoreCherryPickResolveRevisionEventData;
use crate::revision::cherry_pick::LoreCherryPickStartBeginEventData;
use crate::revision::cherry_pick::LoreCherryPickStartEndEventData;
use crate::revision::cherry_pick::LoreCherryPickUnresolveFileEventData;
use crate::revision::cherry_pick::LoreCherryPickUnresolveRevisionEventData;
use crate::revision::diff::LoreRevisionDiffFileEventData;
use crate::revision::history::LoreRevisionHistoryEntryEventData;
use crate::revision::history::LoreRevisionHistoryEventData;
use crate::revision::info::LoreRevisionInfoDeltaEventData;
use crate::revision::info::LoreRevisionInfoEventData;
use crate::revision::restore::LoreRevisionRestoreFileBeginEventData;
use crate::revision::restore::LoreRevisionRestoreFileEndEventData;
use crate::revision::restore::LoreRevisionRestoreFileEventData;
use crate::revision::restore::LoreRevisionRestoreFragmentBeginEventData;
use crate::revision::restore::LoreRevisionRestoreFragmentEndEventData;
use crate::revision::restore::LoreRevisionRestoreFragmentProgressEventData;
use crate::revision::restore::LoreRevisionRestoreRevisionEventData;
use crate::revision::restore::LoreRevisionRestoreSyncBeginEventData;
use crate::revision::restore::LoreRevisionRestoreSyncEndEventData;
use crate::revision::revert::LoreRevertAbortBeginEventData;
use crate::revision::revert::LoreRevertAbortEndEventData;
use crate::revision::revert::LoreRevertConflictFileEventData;
use crate::revision::revert::LoreRevertResolveFileEventData;
use crate::revision::revert::LoreRevertResolveRevisionEventData;
use crate::revision::revert::LoreRevertStartBeginEventData;
use crate::revision::revert::LoreRevertStartEndEventData;
use crate::revision::revert::LoreRevertUnresolveFileEventData;
use crate::revision::revert::LoreRevertUnresolveRevisionEventData;
use crate::revision::sync::LoreRevisionSyncFileEventData;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::revision::sync::LoreRevisionSyncRevisionEventData;
use crate::revision::sync::LoreRevisionSyncTargetEventData;
use crate::shared_store::LoreSharedStoreCreateEventData;
use crate::shared_store::LoreSharedStoreInfoEventData;
use crate::stage::LoreFileStageBeginEventData;
use crate::stage::LoreFileStageEndEventData;
use crate::stage::LoreFileStageFileEventData;
use crate::stage::LoreFileStageProgressEventData;
use crate::stage::LoreFileStageRevisionEventData;
use crate::state::LoreRepositoryStateDumpEventData;
use crate::state::LoreRepositoryStateDumpNodeEventData;
use crate::store::event::LoreStorageCopyItemCompleteEventData;
use crate::store::event::LoreStorageGetDataEventData;
use crate::store::event::LoreStorageGetHeaderEventData;
use crate::store::event::LoreStorageGetItemCompleteEventData;
use crate::store::event::LoreStorageGetMetadataItemCompleteEventData;
use crate::store::event::LoreStorageObliterateItemCompleteEventData;
use crate::store::event::LoreStorageOpenedEventData;
use crate::store::event::LoreStoragePutItemCompleteEventData;
use crate::store::event::LoreStorageUploadItemCompleteEventData;

pub fn convert_event_callback(callback: LoreEventCallbackConfig) -> LoreEventCallback {
    if let Some(func) = callback.func {
        Some(Box::new(move |event: &LoreEvent| unsafe {
            func(event, callback.user_context);
        }))
    } else {
        None
    }
}

pub trait EventError: std::fmt::Display {
    // The error to expose to the user. Defaults to `LoreError::Internal` —
    // the right answer for any error_set whose handleable variants are all
    // mapped to opaque internal events; override for sets that surface
    // user-actionable variants like `LoreError::NotFound`.
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    // The underlying error message as generated by URC library
    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for a generic progress event.
// TODO(vri): Implement with a union to enable command-specific progress events
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreProgressEventData {
    /// Placeholder field; carries no meaningful value.
    pub _unused: u32,
}

/// Borrowed byte slice handed to callbacks.
///
/// The pointer is valid only for the duration of the callback that receives
/// it; callers must copy the bytes if they need them beyond that scope.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct LoreBytes {
    /// Pointer to the start of the byte slice.
    pub ptr: *const core::ffi::c_void,
    /// Number of bytes in the slice.
    pub len: usize,
}

// SAFETY: `LoreBytes` is a borrowed view; the referenced bytes live in a
// buffer owned by the emitter whose lifetime contract is "valid for the
// duration of the callback". Passing a view between threads within that
// lifetime is sound — matches the equivalent contract on `LoreString`.
unsafe impl Send for LoreBytes {}
unsafe impl Sync for LoreBytes {}

impl LoreBytes {
    /// View the referenced bytes as a Rust slice.
    ///
    /// # Safety
    ///
    /// Caller must ensure the emitter's lifetime contract is still upheld
    /// at the call — i.e., the view was just received in a callback and
    /// has not outlived it. A zero-length or null view is always safe.
    pub unsafe fn as_slice(&self) -> &[u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            // SAFETY: upheld by the caller's invocation precondition.
            unsafe { core::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
        }
    }
}

impl PartialEq for LoreBytes {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: `PartialEq` is only meaningfully called by the emitter
        // within the view's lifetime (e.g., event comparisons inside the
        // dispatcher). Zero-length / null is handled by `as_slice`.
        unsafe { self.as_slice() == other.as_slice() }
    }
}

impl serde::Serialize for LoreBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // SAFETY: `Serialize` is driven by the callback path while the
        // view is still live.
        serializer.serialize_bytes(unsafe { self.as_slice() })
    }
}

impl<'de> serde::Deserialize<'de> for LoreBytes {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "LoreBytes cannot be deserialized — it is a borrowed view",
        ))
    }
}

/// Small discriminator enum for per-item terminal events in the
/// content-addressed storage API.
///
/// Narrower than the general library error code — events emitted per
/// put/get/copy/etc. item embed this code so a caller can branch on the
/// common cases cheaply without parsing the companion `LORE_EVENT_ERROR`
/// detail. Variants overlap with the general library error code where they
/// share a meaning.
///
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LoreErrorCode {
    /// No error; the operation succeeded.
    None = 0,
    /// The arguments supplied to the operation were invalid.
    InvalidArguments = 1,
    /// A content-addressable object could not be found in any store.
    AddressNotFound = 2,
    /// An internal error occurred.
    Internal = 3,
    /// The backing store is overloaded; the caller should retry later.
    SlowDown = 4,
}

/// Data for an error event.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreErrorEventData {
    /// The error code, matching one of the FFI error codes.
    pub error_type: u32,
    /// The underlying error message.
    pub error_inner: LoreString,
}

impl LoreErrorEventData {
    pub fn from_inner_error(err: &impl EventError) -> Self {
        Self {
            error_type: err.translated() as u32,
            error_inner: LoreString::from(err.inner()),
        }
    }
}

/// Data for a completion event, marking the end of an operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreCompleteEventData {
    /// The completion status code of the operation.
    pub status: i32,
}

/// Data for a metadata event, carrying a single key and value.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreMetadataEventData {
    /// The metadata key.
    pub key: LoreString,
    /// The metadata value.
    pub value: LoreMetadata,
}

impl LoreMetadataEventData {
    pub fn new(key: &str, value: &[u8], value_type: MetadataType) -> Result<Self, MetadataError> {
        let key = LoreString::from(key);
        let value = match value_type {
            MetadataType::Address => LoreMetadata::Address(Metadata::to_address(value)?),
            MetadataType::Boolean => LoreMetadata::Boolean(Metadata::to_bool(value)? as u8),
            MetadataType::Context => LoreMetadata::Context(Metadata::to_context(value)?),
            MetadataType::Hash => LoreMetadata::Hash(Metadata::to_hash(value)?),
            MetadataType::Numeric => LoreMetadata::Numeric(Metadata::to_u64(value)?),
            MetadataType::String => {
                LoreMetadata::String(LoreString::from(Metadata::to_string(value).ok()))
            }
            MetadataType::Binary => return Err(MetadataError::internal("metadata type mismatch")),
        };

        Ok(LoreMetadataEventData { key, value })
    }
}

/// Data for a log event.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLogEventData {
    /// The severity level of the log message.
    pub level: lore_base::log::LoreLogLevel,
    /// The category of the log message.
    pub category: u32,
    /// The time the message was produced.
    pub timestamp: u64,
    /// The source location that produced the message.
    pub location: LoreString,
    /// The log message text.
    pub message: LoreString,
}

/// Data for an end event, marking the final event of a callback stream.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreEndEventData {
    /// Placeholder field; carries no meaningful value.
    pub unused: u32,
}

/// Data for a maintenance event, carrying an informational message.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreMaintenanceEventData {
    /// The maintenance message text.
    pub message: LoreString,
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
/// An event delivered to a callback. Each variant names a kind of event and
/// carries the data for that event.
#[repr(C, u32)]
#[derive(Clone, PartialEq, Serialize, Deserialize, VariantTypeSize)]
#[serde(tag = "tagName", content = "data", rename_all = "camelCase")]
pub enum LoreEvent {
    // Standard events
    /// A progress update.
    Progress(LoreProgressEventData),
    /// An error.
    Error(LoreErrorEventData),
    /// An operation completed.
    Complete(LoreCompleteEventData),
    /// A metadata key and value.
    Metadata(LoreMetadataEventData),
    /// A log message.
    Log(LoreLogEventData),
    /// The final event of a callback stream.
    End(LoreEndEventData),
    /// A maintenance message.
    Maintenance(LoreMaintenanceEventData),
    // ... Specialized events
    /// An authentication URL for the user to visit.
    AuthUrl(LoreAuthUrlEventData),
    /// Information about the authenticated user.
    AuthUserInfo(LoreAuthUserInfoEventData),
    /// An authentication token for the user.
    AuthUserToken(LoreAuthUserTokenEventData),
    /// The resolved identity of the user.
    AuthIdentity(LoreAuthIdentityEventData),
    /// A branch was created.
    BranchCreate(LoreBranchCreateEventData),
    /// More than one instance of a branch was found.
    BranchMultipleInstance(LoreBranchMultipleInstanceEventData),
    /// A branch was archived.
    BranchArchive(LoreBranchArchiveEventData),
    /// The start of a branch listing.
    BranchListBegin(LoreBranchListBeginEventData),
    /// One entry in a branch listing.
    BranchListEntry(LoreBranchListEntryEventData),
    /// The end of a branch listing.
    BranchListEnd(LoreBranchListEndEventData),
    /// The start of a merge abort.
    BranchMergeAbortBegin(LoreBranchMergeAbortBeginEventData),
    /// The end of a merge abort.
    BranchMergeAbortEnd(LoreBranchMergeAbortEndEventData),
    /// Information about a branch.
    BranchInfo(LoreBranchInfoEventData),
    /// The start of a branch diff.
    BranchDiffBegin(LoreBranchDiffBeginEventData),
    /// The start of the changes in a branch diff.
    BranchDiffChangeBegin(LoreBranchDiffChangeBeginEventData),
    /// One change in a branch diff.
    BranchDiffChange(LoreBranchDiffChangeEventData),
    /// The end of the changes in a branch diff.
    BranchDiffChangeEnd(LoreBranchDiffChangeEndEventData),
    /// The start of the conflicts in a branch diff.
    BranchDiffConflictBegin(LoreBranchDiffConflictBeginEventData),
    /// One conflict in a branch diff.
    BranchDiffConflict(LoreBranchDiffConflictEventData),
    /// The end of the conflicts in a branch diff.
    BranchDiffConflictEnd(LoreBranchDiffConflictEndEventData),
    /// The end of a branch diff.
    BranchDiffEnd(LoreBranchDiffEndEventData),
    /// One entry in a listing of latest branch revisions.
    BranchLatestListEntry(LoreBranchLatestListEntryEventData),
    /// A file in conflict during a merge.
    BranchMergeConflictFile(LoreBranchMergeConflictFileEventData),
    /// A link was skipped during a merge.
    BranchMergeLinkSkipped(crate::branch::merge::LoreBranchMergeLinkSkippedEventData),
    /// A file conflict was marked unresolved during a merge.
    BranchMergeUnresolveFile(LoreBranchMergeUnresolveFileEventData),
    /// A revision was marked unresolved during a merge.
    BranchMergeUnresolveRevision(LoreBranchMergeUnresolveRevisionEventData),
    /// The start of merging changes into a file.
    BranchMergeIntoFileBegin(LoreBranchMergeIntoFileBeginEventData),
    /// Merging changes into a file.
    BranchMergeIntoFile(LoreBranchMergeIntoFileEventData),
    /// The end of merging changes into a file.
    BranchMergeIntoFileEnd(LoreBranchMergeIntoFileEndEventData),
    /// The start of merging a fragment.
    BranchMergeIntoFragmentBegin(LoreBranchMergeIntoFragmentBeginEventData),
    /// Progress while merging a fragment.
    BranchMergeIntoFragmentProgress(LoreBranchMergeIntoFragmentProgressEventData),
    /// The end of merging a fragment.
    BranchMergeIntoFragmentEnd(LoreBranchMergeIntoFragmentEndEventData),
    /// A revision merged into the target.
    BranchMergeIntoRevision(LoreBranchMergeIntoRevisionEventData),
    /// The start of synchronizing data for a merge.
    BranchMergeIntoSyncBegin(LoreBranchMergeIntoSyncBeginEventData),
    /// The end of synchronizing data for a merge.
    BranchMergeIntoSyncEnd(LoreBranchMergeIntoSyncEndEventData),
    /// A file conflict was resolved during a merge.
    BranchMergeResolveFile(LoreBranchMergeResolveFileEventData),
    /// A revision was resolved during a merge.
    BranchMergeResolveRevision(LoreBranchMergeResolveRevisionEventData),
    /// The start of a merge.
    BranchMergeStartBegin(LoreBranchMergeStartBeginEventData),
    /// The end of starting a merge.
    BranchMergeStartEnd(LoreBranchMergeStartEndEventData),
    /// The start of a cherry-pick.
    CherryPickStartBegin(LoreCherryPickStartBeginEventData),
    /// The end of starting a cherry-pick.
    CherryPickStartEnd(LoreCherryPickStartEndEventData),
    /// The start of a cherry-pick abort.
    CherryPickAbortBegin(LoreCherryPickAbortBeginEventData),
    /// The end of a cherry-pick abort.
    CherryPickAbortEnd(LoreCherryPickAbortEndEventData),
    /// A file in conflict during a cherry-pick.
    CherryPickConflictFile(LoreCherryPickConflictFileEventData),
    /// A file conflict was marked unresolved during a cherry-pick.
    CherryPickUnresolveFile(LoreCherryPickUnresolveFileEventData),
    /// A revision was marked unresolved during a cherry-pick.
    CherryPickUnresolveRevision(LoreCherryPickUnresolveRevisionEventData),
    /// A file conflict was resolved during a cherry-pick.
    CherryPickResolveFile(LoreCherryPickResolveFileEventData),
    /// A revision was resolved during a cherry-pick.
    CherryPickResolveRevision(LoreCherryPickResolveRevisionEventData),
    /// The start of a revert.
    RevertStartBegin(LoreRevertStartBeginEventData),
    /// The end of starting a revert.
    RevertStartEnd(LoreRevertStartEndEventData),
    /// The start of a revert abort.
    RevertAbortBegin(LoreRevertAbortBeginEventData),
    /// The end of a revert abort.
    RevertAbortEnd(LoreRevertAbortEndEventData),
    /// A file conflict was resolved during a revert.
    RevertResolveFile(LoreRevertResolveFileEventData),
    /// A revision was resolved during a revert.
    RevertResolveRevision(LoreRevertResolveRevisionEventData),
    /// A file in conflict during a revert.
    RevertConflictFile(LoreRevertConflictFileEventData),
    /// A file conflict was marked unresolved during a revert.
    RevertUnresolveFile(LoreRevertUnresolveFileEventData),
    /// A revision was marked unresolved during a revert.
    RevertUnresolveRevision(LoreRevertUnresolveRevisionEventData),
    /// A branch was protected.
    BranchProtect(LoreBranchProtectEventData),
    /// A branch was pushed.
    BranchPush(LoreBranchPushEventData),
    /// The start of updating a revision during a push.
    BranchPushRevisionUpdateBegin(LoreBranchPushRevisionUpdateBeginEventData),
    /// The end of updating a revision during a push.
    BranchPushRevisionUpdateEnd(LoreBranchPushRevisionUpdateEndEventData),
    /// The start of pushing a fragment.
    BranchPushFragmentBegin(LoreBranchPushFragmentBeginEventData),
    /// Progress while pushing a fragment.
    BranchPushFragmentProgress(LoreBranchPushFragmentProgressEventData),
    /// The end of pushing a fragment.
    BranchPushFragmentEnd(LoreBranchPushFragmentEndEventData),
    /// The start of creating a branch during a push.
    BranchPushBranchCreateBegin(LoreBranchPushBranchCreateBeginEventData),
    /// The end of creating a branch during a push.
    BranchPushBranchCreateEnd(LoreBranchPushBranchCreateEndEventData),
    /// The start of pushing a revision.
    BranchPushRevisionPushBegin(LoreBranchPushRevisionPushBeginEventData),
    /// An update while pushing a revision.
    BranchPushRevisionPushUpdate(LoreBranchPushRevisionPushUpdateEventData),
    /// The end of pushing a revision.
    BranchPushRevisionPushEnd(LoreBranchPushRevisionPushEndEventData),
    /// A branch was reset.
    BranchReset(LoreBranchResetEventData),
    /// The start of switching the active branch.
    BranchSwitchBegin(LoreBranchSwitchBeginEventData),
    /// The end of switching the active branch.
    BranchSwitchEnd(LoreBranchSwitchEndEventData),
    /// A branch was unprotected.
    BranchUnprotect(LoreBranchUnprotectEventData),
    /// Information about a file.
    FileInfo(LoreFileInfoEventData),
    /// A diff for a file.
    FileDiff(LoreFileDiffEventData),
    /// The hash of a file.
    FileHash(LoreFileHashEventData),
    /// The history of a file.
    FileHistory(LoreFileHistoryEventData),
    /// A file was written.
    FileWrite(LoreFileWriteEventData),
    /// A file was obliterated.
    FileObliterate(LoreFileObliterateEventData),
    /// A dump of a file.
    FileDump(LoreFileDumpEventData),
    /// The start of adding file dependencies.
    FileDependencyAddBegin(LoreFileDependencyAddBeginEventData),
    /// One entry while adding file dependencies.
    FileDependencyAddEntry(LoreFileDependencyAddEntryEventData),
    /// The end of adding file dependencies.
    FileDependencyAddEnd(LoreFileDependencyAddEndEventData),
    /// The start of removing file dependencies.
    FileDependencyRemoveBegin(LoreFileDependencyRemoveBeginEventData),
    /// One entry while removing file dependencies.
    FileDependencyRemoveEntry(LoreFileDependencyRemoveEntryEventData),
    /// The end of removing file dependencies.
    FileDependencyRemoveEnd(LoreFileDependencyRemoveEndEventData),
    /// The start of listing file dependencies.
    FileDependencyListBegin(LoreFileDependencyListBeginEventData),
    /// A file in a dependency listing.
    FileDependencyListFile(LoreFileDependencyListFileEventData),
    /// One entry in a file dependency listing.
    FileDependencyListEntry(LoreFileDependencyListEntryEventData),
    /// The end of the entries for one file in a dependency listing.
    FileDependencyListFileEnd(LoreFileDependencyListFileEndEventData),
    /// The end of listing file dependencies.
    FileDependencyListEnd(LoreFileDependencyListEndEventData),
    /// The start of a file reset.
    FileResetBegin(LoreFileResetBeginEventData),
    /// Progress during a file reset.
    FileResetProgress(LoreFileResetProgressEventData),
    /// The end of a file reset.
    FileResetEnd(LoreFileResetEndEventData),
    /// One file reset.
    FileResetFile(LoreFileResetFileEventData),
    /// A path was excluded by a filter.
    FilterExclude(LoreFilterExcludeEventData),
    /// The start of staging files.
    FileStageBegin(LoreFileStageBeginEventData),
    /// Progress while staging files.
    FileStageProgress(LoreFileStageProgressEventData),
    /// The end of staging files.
    FileStageEnd(LoreFileStageEndEventData),
    /// The revision involved in staging files.
    FileStageRevision(LoreFileStageRevisionEventData),
    /// One file staged.
    FileStageFile(LoreFileStageFileEventData),
    /// The start of unstaging files.
    FileUnstageBegin(LoreFileUnstageBeginEventData),
    /// Progress while unstaging files.
    FileUnstageProgress(LoreFileUnstageProgressEventData),
    /// The end of unstaging files.
    FileUnstageEnd(LoreFileUnstageEndEventData),
    /// The revision involved in unstaging files.
    FileUnstageRevision(LoreFileUnstageRevisionEventData),
    /// One file unstaged.
    FileUnstageFile(LoreFileUnstageFileEventData),
    /// A fragment was written.
    FragmentWrite(LoreFragmentWriteEventData),
    /// A layer was added.
    LayerAdd(LoreLayerAddEventData),
    /// One entry in a layer listing.
    LayerEntry(LoreLayerEntryEventData),
    /// A layer was removed.
    LayerRemove(LoreLayerRemoveEventData),
    /// One staged entry in a layer listing.
    LayerStagedEntry(LoreLayerStagedEntryEventData),
    /// A link was changed.
    LinkChange(LoreLinkChangeEventData),
    /// One entry in a link listing.
    LinkEntry(LoreLinkEntryEventData),
    /// A file lock was acquired.
    LockFileAcquire(LoreLockFileAcquireEventData),
    /// A file lock acquisition was ignored.
    LockFileAcquireIgnore(LoreLockFileAcquireIgnoreEventData),
    /// The start of a file lock status report.
    LockFileStatusBegin(LoreLockFileStatusBeginEventData),
    /// One file lock status entry.
    LockFileStatus(LoreLockFileStatusEventData),
    /// The start of a file lock query.
    LockFileQueryBegin(LoreLockFileQueryBeginEventData),
    /// One file lock query result.
    LockFileQuery(LoreLockFileQueryEventData),
    /// A file lock was released.
    LockFileRelease(LoreLockFileReleaseEventData),
    /// A file lock to release was not found.
    LockFileReleaseNotFound(LoreLockFileReleaseNotFoundEventData),
    /// Metadata was cleared on a file.
    MetadataClearFile(LoreMetadataClearFileEventData),
    /// Metadata was cleared on a revision.
    MetadataClearRevision(LoreMetadataClearRevisionEventData),
    /// A path was ignored.
    PathIgnore(LorePathIgnoreEventData),
    /// A repository was created.
    RepositoryCreate(LoreRepositoryCreateEventData),
    /// The start of a repository clone.
    RepositoryCloneBegin(LoreRepositoryCloneBeginEventData),
    /// Progress during a repository clone.
    RepositoryCloneProgress(LoreRepositoryCloneProgressEventData),
    /// The end of a repository clone.
    RepositoryCloneEnd(LoreRepositoryCloneEndEventData),
    /// The start of resolving dependencies.
    DependencyResolveBegin(LoreDependencyResolveBeginEventData),
    /// One item while resolving dependencies.
    DependencyResolveItem(LoreDependencyResolveItemEventData),
    /// The end of resolving dependencies.
    DependencyResolveEnd(LoreDependencyResolveEndEventData),
    /// Data about a repository.
    RepositoryData(LoreRepositoryDataEventData),
    /// A repository configuration value.
    RepositoryConfigGet(LoreRepositoryConfigGetEventData),
    /// The start of a repository dump.
    RepositoryDumpBegin(LoreRepositoryDumpBeginEventData),
    /// The end of a repository dump.
    RepositoryDumpEnd(LoreRepositoryDumpEndEventData),
    /// One entry in a repository listing.
    RepositoryListEntry(LoreRepositoryListEntryEventData),
    /// An instance of a repository.
    RepositoryInstance(LoreRepositoryInstanceEventData),
    /// The start of verifying repository state.
    RepositoryVerifyStateBegin(LoreRepositoryVerifyStateBeginEventData),
    /// The end of verifying repository state.
    RepositoryVerifyStateEnd(LoreRepositoryVerifyStateEndEventData),
    /// A fragment verified in a repository.
    RepositoryVerifyFragment(LoreRepositoryVerifyFragmentEventData),
    /// A fragment match found while verifying a repository.
    RepositoryVerifyFragmentMatch(LoreRepositoryVerifyFragmentMatchEventData),
    /// A remote fragment checked while verifying a repository.
    RepositoryVerifyFragmentRemote(LoreRepositoryVerifyFragmentRemoteEventData),
    /// A dump of repository state.
    RepositoryStateDump(LoreRepositoryStateDumpEventData),
    /// One node in a repository state dump.
    RepositoryStateDumpNode(LoreRepositoryStateDumpNodeEventData),
    /// The revision involved in a repository status report.
    RepositoryStatusRevision(LoreRepositoryStatusRevisionEventData),
    /// One file in a repository status report.
    RepositoryStatusFile(LoreRepositoryStatusFileEventData),
    /// File counts in a repository status report.
    RepositoryStatusCount(LoreRepositoryStatusCountEventData),
    /// A summary of a repository status report.
    RepositoryStatusSummary(LoreRepositoryStatusSummaryEventData),
    /// A result from querying the immutable store.
    RepositoryStoreImmutableQuery(LoreRepositoryStoreImmutableQueryEventData),
    /// The start of committing a revision.
    RevisionCommitBegin(LoreRevisionCommitBeginEventData),
    /// Progress while committing a revision.
    RevisionCommitProgress(LoreRevisionCommitProgressEventData),
    /// The end of committing a revision.
    RevisionCommitEnd(LoreRevisionCommitEndEventData),
    /// The committed revision.
    RevisionCommitRevision(LoreRevisionCommitRevisionEventData),
    /// Information about a revision.
    RevisionInfo(LoreRevisionInfoEventData),
    /// A change in a revision's delta.
    RevisionInfoDelta(LoreRevisionInfoDeltaEventData),
    /// One file in a revision diff.
    RevisionDiffFile(LoreRevisionDiffFileEventData),
    /// A revision found by a search.
    RevisionFind(LoreRevisionFindEventData),
    /// The history of a revision.
    RevisionHistory(LoreRevisionHistoryEventData),
    /// One entry in a revision history.
    RevisionHistoryEntry(LoreRevisionHistoryEntryEventData),
    /// The start of restoring a file from a revision.
    RevisionRestoreFileBegin(LoreRevisionRestoreFileBeginEventData),
    /// A file restored from a revision.
    RevisionRestoreFile(LoreRevisionRestoreFileEventData),
    /// The end of restoring a file from a revision.
    RevisionRestoreFileEnd(LoreRevisionRestoreFileEndEventData),
    /// The start of restoring a fragment.
    RevisionRestoreFragmentBegin(LoreRevisionRestoreFragmentBeginEventData),
    /// Progress while restoring a fragment.
    RevisionRestoreFragmentProgress(LoreRevisionRestoreFragmentProgressEventData),
    /// The end of restoring a fragment.
    RevisionRestoreFragmentEnd(LoreRevisionRestoreFragmentEndEventData),
    /// The revision being restored.
    RevisionRestoreRevision(LoreRevisionRestoreRevisionEventData),
    /// The start of synchronizing data for a restore.
    RevisionRestoreSyncBegin(LoreRevisionRestoreSyncBeginEventData),
    /// The end of synchronizing data for a restore.
    RevisionRestoreSyncEnd(LoreRevisionRestoreSyncEndEventData),
    /// A revision was resolved.
    RevisionResolve(LoreRevisionResolveEventData),
    /// The target revision of a sync.
    RevisionSyncTarget(LoreRevisionSyncTargetEventData),
    /// One file synced.
    RevisionSyncFile(LoreRevisionSyncFileEventData),
    /// Progress during a revision sync.
    RevisionSyncProgress(LoreRevisionSyncProgressEventData),
    /// The revision involved in a sync.
    RevisionSyncRevision(LoreRevisionSyncRevisionEventData),
    /// A bisect result.
    RevisionBisect(LoreRevisionBisectEventData),
    /// A notification that a branch was created.
    NotificationBranchCreated(LoreNotificationBranchCreatedEventData),
    /// A notification that a branch was deleted.
    NotificationBranchDeleted(LoreNotificationBranchDeletedEventData),
    /// A notification that a branch was pushed.
    NotificationBranchPushed(LoreNotificationBranchPushedEventData),
    /// A notification that a resource was locked.
    NotificationResourceLocked(LoreNotificationResourceLockedEventData),
    /// A notification that a resource was unlocked.
    NotificationResourceUnlocked(LoreNotificationResourceUnlockedEventData),
    /// A notification that a subscription was created.
    NotificationSubscribed(LoreNotificationSubscribedEventData),
    /// A notification that a subscription was removed.
    NotificationUnsubscribed(LoreNotificationUnsubscribedEventData),
    /// A shared store was created.
    SharedStoreCreate(LoreSharedStoreCreateEventData),
    /// Information about a shared store.
    SharedStoreInfo(LoreSharedStoreInfoEventData),
    /// One staged entry in a link listing.
    LinkStagedEntry(LoreLinkStagedEntryEventData),
    // Content-addressed storage API
    /// A store was opened.
    StorageOpened(LoreStorageOpenedEventData),
    /// A put item completed.
    StoragePutItemComplete(LoreStoragePutItemCompleteEventData),
    /// The header for a get item.
    StorageGetHeader(LoreStorageGetHeaderEventData),
    /// A data payload for a get item.
    StorageGetData(LoreStorageGetDataEventData),
    /// A get item completed.
    StorageGetItemComplete(LoreStorageGetItemCompleteEventData),
    /// A get-metadata item completed.
    StorageGetMetadataItemComplete(LoreStorageGetMetadataItemCompleteEventData),
    /// A copy item completed.
    StorageCopyItemComplete(LoreStorageCopyItemCompleteEventData),
    /// An obliterate item completed.
    StorageObliterateItemComplete(LoreStorageObliterateItemCompleteEventData),
    /// An upload item completed.
    StorageUploadItemComplete(LoreStorageUploadItemCompleteEventData),
    // Low-level memory-based revision control API
    /// A revision tree was loaded.
    RevisionTreeLoaded(LoreRevisionTreeLoadedEventData),
    /// A resolve-path call completed.
    RevisionTreeResolvePathComplete(LoreRevisionTreeResolvePathCompleteEventData),
    /// One child node in a revision tree.
    RevisionTreeChild(LoreRevisionTreeChildEventData),
    /// Information about a revision tree node.
    RevisionTreeNodeInfo(LoreRevisionTreeNodeInfoEventData),
    /// The path of a revision tree node.
    RevisionTreeNodePath(LoreRevisionTreeNodePathEventData),
    /// An add call completed.
    RevisionTreeAddComplete(LoreRevisionTreeAddCompleteEventData),
    /// A delete call completed.
    RevisionTreeDeleteComplete(LoreRevisionTreeDeleteCompleteEventData),
    /// A modify call completed.
    RevisionTreeModifyComplete(LoreRevisionTreeModifyCompleteEventData),
    /// A move call completed.
    RevisionTreeMoveComplete(LoreRevisionTreeMoveCompleteEventData),
    /// A metadata-set call completed.
    RevisionTreeMetadataSetComplete(LoreRevisionTreeMetadataSetCompleteEventData),
    /// A metadata-get call completed.
    RevisionTreeMetadataGetComplete(LoreRevisionTreeMetadataGetCompleteEventData),
    /// A commit call completed.
    RevisionTreeCommitComplete(LoreRevisionTreeCommitCompleteEventData),
    /// A close call completed.
    RevisionTreeCloseComplete(LoreRevisionTreeCloseCompleteEventData),
}

impl LoreEvent {
    pub fn send(self) {
        execution_context().dispatcher.send(self);
    }

    pub fn discriminant(&self) -> u32 {
        // SAFETY: Because `Self` is marked `repr(u32)`, its layout is a `repr(C)` `union`
        // between `repr(C)` structs, each of which has the `u32` discriminant as its first
        // field, so we can read the discriminant without offsetting the pointer.
        unsafe {
            let ptr = <*const Self>::from(self).cast::<u32>();
            if ptr.is_aligned() {
                *ptr
            } else {
                ptr.read_unaligned()
            }
        }
    }
}
