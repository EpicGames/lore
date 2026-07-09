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
use crate::event::revision_tree::LoreRevisionTreeInfoEventData;
use crate::event::revision_tree::LoreRevisionTreeListChildrenBeginEventData;
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
use crate::interface::LoreArray;
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
use crate::lock::file::acquire::LoreLockFileAcquireBeginEventData;
use crate::lock::file::acquire::LoreLockFileAcquireEventData;
use crate::lock::file::query::LoreLockFileQueryBeginEventData;
use crate::lock::file::query::LoreLockFileQueryEventData;
use crate::lock::file::release::LoreLockFileReleaseBeginEventData;
use crate::lock::file::release::LoreLockFileReleaseEventData;
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
use crate::store::event::LoreStorageMutableCompareAndSwapItemCompleteEventData;
use crate::store::event::LoreStorageMutableListEntryEventData;
use crate::store::event::LoreStorageMutableListItemCompleteEventData;
use crate::store::event::LoreStorageMutableLoadItemCompleteEventData;
use crate::store::event::LoreStorageMutableStoreItemCompleteEventData;
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
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum LoreErrorCode {
    /// No error; the operation succeeded.
    #[default]
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
    /// The error code, matching one of the error codes.
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
    /// The error detail for the operation. The empty default detail on
    /// success; the populated detail on failure. `#[serde(default)]` lets an
    /// older payload that lacks this field deserialize: the detail then reads
    /// back as the empty default with an empty trace list.
    #[serde(default)]
    pub error: LoreErrorDetail,
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

/// One captured trace entry, carried across the FFI boundary as structured
/// data.
///
/// It records the source location where an error was created or forwarded:
/// the file path, line, column, and an optional per-location context string.
/// The struct owns its `file` and `context` strings. `Clone` deep-clones them
/// and `Drop` frees them.
///
/// Memory: the library owns this data. The pointers a consumer reads from this
/// struct are valid only for the single callback invocation that delivers the
/// event. A consumer that keeps any of this data must copy it out before the
/// callback returns.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreTraceLocation {
    /// The source file path.
    pub file: LoreString,
    /// The line number in the source file.
    pub line: u32,
    /// The column number in the source file.
    pub column: u32,
    /// The context describing the operation at this location, or an empty
    /// string when the location has none.
    pub context: LoreString,
}

impl LoreTraceLocation {
    /// Builds a trace location from a `lore-error-set` [`Location`], copying
    /// its file and context into owned [`LoreString`]s. A location with no
    /// context yields an empty `context` string.
    ///
    /// [`Location`]: lore_error_set::Location
    pub fn from_location(location: &lore_error_set::Location) -> Self {
        Self {
            file: LoreString::from(location.file),
            line: location.line,
            column: location.column,
            context: LoreString::from(location.context()),
        }
    }
}

impl std::fmt::Display for LoreTraceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.context.is_empty() {
            write!(f, "{}:{}:{}", self.file, self.line, self.column)
        } else {
            write!(f, "{}:{} - {}", self.file, self.line, self.context)
        }
    }
}

/// The shared error payload carried on a failed operation.
///
/// Every consumer reads this on a failure. It holds the error's error code, the
/// error message, and the captured trace as structured data. `Default` yields
/// the empty detail used on success: code `0`, an empty message, and an empty
/// trace array.
///
/// The number of trace locations is bounded by the trace capacity in
/// `lore-error-set` ([`MAX_TRACE_DEPTH`]). The trace array is empty when the
/// `track-locations` feature is off or when the error carries no trace.
///
/// Memory: the library owns this data. The pointers a consumer reads from this
/// struct (the `message` string and the `trace_locations` array, and the
/// strings inside each location) are valid only for the single callback
/// invocation that delivers the event. A consumer that keeps any of this data
/// must copy it out before the callback returns.
///
/// [`MAX_TRACE_DEPTH`]: lore_error_set::MAX_TRACE_DEPTH
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreErrorDetail {
    /// The error's error code. `0` on success; `-1` for an internal error.
    #[serde(default)]
    pub error_code: i32,
    /// The error message, taken from the error's `Display` output. Empty on
    /// success.
    #[serde(default)]
    pub message: LoreString,
    /// The captured trace, one location per trace entry. Empty when
    /// `track-locations` is off or the error carries no trace.
    #[serde(default)]
    pub trace_locations: LoreArray<LoreTraceLocation>,
}

impl LoreErrorDetail {
    /// Renders the message followed by the captured trace, one indented line
    /// per location, for human-readable logging. With no trace it is just the
    /// message.
    pub fn message_with_trace(&self) -> String {
        use std::fmt::Write as _;
        let mut text = self.message.as_str().to_string();
        for location in self.trace_locations.as_slice() {
            let _ = write!(text, "\n  at {location}");
        }
        text
    }

    /// Builds an error detail from a concrete `#[error_set]` error, reading its
    /// own captured trace.
    ///
    /// The error supplies its error code through [`FfiError::ffi_code`], its
    /// message through [`Display`], and its trace through [`HasTrace`]. The
    /// trace supplies one [`LoreTraceLocation`] per recorded location. When
    /// `track-locations` is off the trace reports no locations, so the array is
    /// empty.
    ///
    /// [`FfiError::ffi_code`]: lore_error_set::FfiError::ffi_code
    /// [`HasTrace`]: lore_error_set::HasTrace
    pub fn from_error<E>(error: &E) -> Self
    where
        E: lore_error_set::FfiError + std::fmt::Display + lore_error_set::HasTrace,
    {
        Self::from_error_with_trace(error, error.trace())
    }

    /// Builds an error detail from an error and an explicitly supplied trace.
    fn from_error_with_trace<E>(error: &E, trace: &lore_error_set::Trace) -> Self
    where
        E: lore_error_set::FfiError + std::fmt::Display,
    {
        let trace_locations = trace
            .locations()
            .iter()
            .map(LoreTraceLocation::from_location)
            .collect::<Vec<_>>();

        Self {
            error_code: error.ffi_code(),
            message: LoreString::from(error.to_string()),
            trace_locations: LoreArray::from_vec(trace_locations),
        }
    }

    /// Builds the detail for a command result: the empty success detail on
    /// `Ok`, the populated detail (read from the error's own trace) on `Err`.
    pub fn from_result<T, E>(result: Result<T, E>) -> Self
    where
        E: lore_error_set::FfiError + std::fmt::Display + lore_error_set::HasTrace,
    {
        match result {
            Ok(_) => Self::default(),
            Err(err) => Self::from_error(&err),
        }
    }
}

/// Data for the start of a store eviction pass.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreEvictionBeginEventData {
    /// Fragment capacity the pass is reducing the store toward.
    pub target_fragments: u64,
}

/// Data for one bucket evicted during a store eviction pass.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreEvictionProgressEventData {
    /// Fragments evicted from this bucket.
    pub evicted: u64,
}

/// Data for the end of a store eviction pass.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreEvictionEndEventData {
    /// Total fragments evicted across the pass.
    pub total_evicted: u64,
}

/// Data for the start of a store compaction pass.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreCompactionBeginEventData {
    /// Store size in bytes the pass is reducing the store toward.
    pub target_bytes: u64,
}

/// Data for one group compacted during a store compaction pass.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreCompactionProgressEventData {
    /// Bytes reclaimed from this group.
    pub compacted_bytes: u64,
}

/// Data for the end of a store compaction pass.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreCompactionEndEventData {
    /// Total bytes reclaimed across the pass.
    pub total_compacted_bytes: u64,
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
    /// An error encountered during an operation. A terminal failure is
    /// reported on the `Complete` event in its `error` field.
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
    /// The start of a file lock acquire report.
    LockFileAcquireBegin(LoreLockFileAcquireBeginEventData),
    /// A file concerning the lock acquire report.
    LockFileAcquire(LoreLockFileAcquireEventData),
    /// The start of a file lock status report.
    LockFileStatusBegin(LoreLockFileStatusBeginEventData),
    /// One file lock status entry.
    LockFileStatus(LoreLockFileStatusEventData),
    /// The start of a file lock query.
    LockFileQueryBegin(LoreLockFileQueryBeginEventData),
    /// One file lock query result.
    LockFileQuery(LoreLockFileQueryEventData),
    /// The start of a file lock release report.
    LockFileReleaseBegin(LoreLockFileReleaseBeginEventData),
    /// A file concerning the lock release report.
    LockFileRelease(LoreLockFileReleaseEventData),
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
    /// A list-children call began; carries the target repository and revision.
    RevisionTreeListChildrenBegin(LoreRevisionTreeListChildrenBeginEventData),
    /// Revision-record metadata for a loaded revision tree.
    RevisionTreeInfo(LoreRevisionTreeInfoEventData),
    // Mutable-store API events are appended here rather than grouped with the other storage
    // events above so that adding them does not renumber the `#[repr(C, u32)]` discriminants of
    // the existing variants — new variants go at the end of this enum.
    /// A mutable-load item completed.
    StorageMutableLoadItemComplete(LoreStorageMutableLoadItemCompleteEventData),
    /// A mutable-store item completed.
    StorageMutableStoreItemComplete(LoreStorageMutableStoreItemCompleteEventData),
    /// A mutable-compare-and-swap item completed.
    StorageMutableCompareAndSwapItemComplete(LoreStorageMutableCompareAndSwapItemCompleteEventData),
    /// One key-value entry in a mutable listing.
    StorageMutableListEntry(LoreStorageMutableListEntryEventData),
    /// A mutable-list item completed.
    StorageMutableListItemComplete(LoreStorageMutableListItemCompleteEventData),
    /// A store eviction pass began.
    EvictionBegin(LoreEvictionBeginEventData),
    /// One bucket was evicted during a store eviction pass.
    EvictionProgress(LoreEvictionProgressEventData),
    /// A store eviction pass ended.
    EvictionEnd(LoreEvictionEndEventData),
    /// A store compaction pass began.
    CompactionBegin(LoreCompactionBeginEventData),
    /// One group was compacted during a store compaction pass.
    CompactionProgress(LoreCompactionProgressEventData),
    /// A store compaction pass ended.
    CompactionEnd(LoreCompactionEndEventData),
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

#[cfg(test)]
mod trace_location_tests {
    use std::sync::Arc;

    use lore_error_set::Location;

    use super::LoreTraceLocation;

    #[test]
    fn builds_from_location_with_context() {
        let location = Location::with_context("src/main.rs", 42, 7, Arc::from("loading config"));

        let trace = LoreTraceLocation::from_location(&location);

        assert_eq!(trace.file.as_str(), "src/main.rs");
        assert_eq!(trace.line, 42);
        assert_eq!(trace.column, 7);
        assert_eq!(trace.context.as_str(), "loading config");
    }

    #[test]
    fn builds_from_location_without_context_yields_empty_context() {
        let location = Location::new("src/lib.rs", 1, 1);

        let trace = LoreTraceLocation::from_location(&location);

        assert_eq!(trace.file.as_str(), "src/lib.rs");
        assert_eq!(trace.line, 1);
        assert_eq!(trace.column, 1);
        assert!(trace.context.is_empty());
        assert_eq!(trace.context.as_str(), "");
    }

    #[test]
    fn clone_is_independent_deep_copy_and_both_drop_cleanly() {
        let location = Location::with_context("src/clone.rs", 3, 9, Arc::from("deep copy"));
        let trace = LoreTraceLocation::from_location(&location);

        let clone = trace.clone();

        // The clone holds its own allocations, not shared pointers.
        assert_ne!(trace.file.string, clone.file.string);
        assert_ne!(trace.context.string, clone.context.string);

        // The clone is value-equal to the original. `LoreTraceLocation` does
        // not derive `Debug`, so compare through `PartialEq` directly.
        assert!(trace == clone);
        assert_eq!(clone.file.as_str(), "src/clone.rs");
        assert_eq!(clone.context.as_str(), "deep copy");

        // Dropping the original must not affect the clone's strings.
        drop(trace);
        assert_eq!(clone.file.as_str(), "src/clone.rs");
        assert_eq!(clone.context.as_str(), "deep copy");

        // Dropping the clone frees its own strings; under leak detection this
        // confirms no double free and no leak.
        drop(clone);
    }

    #[test]
    fn displays_file_line_column_without_context() {
        let trace = LoreTraceLocation::from_location(&Location::new("src/lib.rs", 12, 4));
        assert_eq!(trace.to_string(), "src/lib.rs:12:4");
    }

    #[test]
    fn displays_context_in_place_of_column_when_present() {
        let location = Location::with_context("src/main.rs", 7, 2, Arc::from("loading config"));
        let trace = LoreTraceLocation::from_location(&location);
        assert_eq!(trace.to_string(), "src/main.rs:7 - loading config");
    }
}

#[cfg(test)]
mod error_detail_tests {
    use lore_base::error::NotFound;
    use lore_error_set::FfiError;
    use lore_error_set::Location;
    use lore_error_set::Trace;
    use lore_error_set::prelude::*;

    use super::LoreErrorDetail;

    // A concrete `#[error_set]` error used to exercise the constructor. Its
    // `NotFound` variant wraps `lore_base::error::NotFound`, which carries FFI
    // code 13, so the detail's `error_code` has a known, non-internal value to
    // assert against.
    #[error_set]
    enum SampleError {
        NotFound,
    }

    #[test]
    fn from_error_holds_code_message_and_one_location_per_trace_entry() {
        // Build a concrete error and give it a trace with two entries.
        let mut error: SampleError = NotFound.into();
        error.push_trace(Location::new("src/first.rs", 10, 2));
        error.push_trace(Location::new("src/second.rs", 20, 4));

        let detail = LoreErrorDetail::from_error(&error);

        // The code is the error's error code.
        assert_eq!(detail.error_code, error.ffi_code());
        // The message is the error's `Display` output.
        assert_eq!(detail.message.as_str(), error.to_string());

        // One trace location per trace entry. The `From` conversion adds its
        // own caller location ahead of the two we pushed, so the count and
        // the contents must match the trace exactly.
        let locations = error.trace().locations();
        assert_eq!(detail.trace_locations.len(), locations.len());
        for (built, source) in detail.trace_locations.as_slice().iter().zip(locations) {
            assert_eq!(built.file.as_str(), source.file);
            assert_eq!(built.line, source.line);
            assert_eq!(built.column, source.column);
        }
    }

    #[test]
    fn default_is_the_empty_success_detail() {
        let detail = LoreErrorDetail::default();

        assert_eq!(detail.error_code, 0);
        assert!(detail.message.is_empty());
        assert!(detail.trace_locations.is_empty());
    }

    #[test]
    fn error_set_enum_exposes_trace_through_has_trace_bound() {
        use lore_error_set::HasTrace;

        // A generic function can only read the trace through the bound, not the
        // inherent method, so this exercises the trait `#[error_set]` generates.
        fn locations_through_bound<E: HasTrace>(error: &E) -> usize {
            error.trace().locations().len()
        }

        let mut error: SampleError = NotFound.into();
        error.push_trace(Location::new("src/bound.rs", 5, 1));

        // The trait access matches the inherent access on the same error.
        assert_eq!(
            locations_through_bound(&error),
            error.trace().locations().len()
        );
    }

    #[test]
    fn empty_trace_yields_empty_location_array() {
        // An empty trace is the observable behavior when `track-locations` is
        // off: `Trace::locations()` reports no entries, so the array is empty
        // and the path stays safe. With the feature on, an error built with an
        // empty trace exercises the same empty-array path.
        let error: SampleError = NotFound.into();
        let empty_trace = Trace::new();

        let detail = LoreErrorDetail::from_error_with_trace(&error, &empty_trace);

        assert!(detail.trace_locations.is_empty());
        assert_eq!(detail.error_code, error.ffi_code());
    }

    #[test]
    fn message_with_trace_appends_one_indented_line_per_location() {
        use super::LoreTraceLocation;
        use crate::interface::LoreArray;
        use crate::interface::LoreString;

        let detail = LoreErrorDetail {
            error_code: 13,
            message: LoreString::from("not found"),
            trace_locations: LoreArray::from_vec(vec![
                LoreTraceLocation {
                    file: LoreString::from("src/a.rs"),
                    line: 10,
                    column: 2,
                    context: LoreString::default(),
                },
                LoreTraceLocation {
                    file: LoreString::from("src/b.rs"),
                    line: 20,
                    column: 4,
                    context: LoreString::from("loading"),
                },
            ]),
        };

        assert_eq!(
            detail.message_with_trace(),
            "not found\n  at src/a.rs:10:2\n  at src/b.rs:20 - loading"
        );
    }

    #[test]
    fn message_with_trace_is_just_the_message_when_no_trace() {
        use crate::interface::LoreString;

        let detail = LoreErrorDetail {
            error_code: 13,
            message: LoreString::from("boom"),
            trace_locations: Default::default(),
        };

        assert_eq!(detail.message_with_trace(), "boom");
    }
}

#[cfg(test)]
mod complete_event_tests {
    use super::LoreCompleteEventData;
    use super::LoreErrorDetail;
    use super::LoreTraceLocation;
    use crate::interface::LoreArray;
    use crate::interface::LoreString;

    // Builds a populated error detail with one trace location so the
    // serialized form carries non-default values to assert against.
    fn populated_detail() -> LoreErrorDetail {
        let location = LoreTraceLocation {
            file: LoreString::from("src/op.rs"),
            line: 11,
            column: 5,
            context: LoreString::from("running op"),
        };

        LoreErrorDetail {
            error_code: 13,
            message: LoreString::from("not found"),
            trace_locations: LoreArray::from_vec(vec![location]),
        }
    }

    #[test]
    fn serializes_error_detail_fields_in_camel_case() {
        let event = LoreCompleteEventData {
            status: 13,
            error: populated_detail(),
        };

        let json: serde_json::Value = serde_json::to_value(&event).unwrap();

        // The `status` field keeps its key and value.
        assert_eq!(json["status"], 13);

        // The appended detail nests under `error` and uses camelCase keys for
        // its own fields.
        let error = &json["error"];
        assert_eq!(error["errorCode"], 13);
        assert_eq!(error["message"], "not found");

        let traces = error["traceLocations"].as_array().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0]["file"], "src/op.rs");
        assert_eq!(traces[0]["line"], 11);
        assert_eq!(traces[0]["column"], 5);
        assert_eq!(traces[0]["context"], "running op");
    }

    #[test]
    fn deserializes_old_payload_without_error_detail() {
        // A serialized payload that carries only the `status` field.
        let json = r#"{ "status": 7 }"#;

        let event: LoreCompleteEventData = serde_json::from_str(json).unwrap();

        // The status is read back unchanged.
        assert_eq!(event.status, 7);

        // The missing detail defaults to the empty success detail: code 0,
        // empty message, and an empty trace list.
        assert_eq!(event.error.error_code, 0);
        assert!(event.error.message.is_empty());
        assert!(event.error.trace_locations.is_empty());
    }

    #[test]
    fn legacy_status_field_keeps_its_name_position_and_type() {
        // The `status` field serializes under its existing key.
        let event = LoreCompleteEventData {
            status: 42,
            error: LoreErrorDetail::default(),
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json["status"], 42);

        // `status` keeps its `i32` type: an `i32` binds directly into the
        // first field, so a change of type or position would fail to compile.
        let status: i32 = -1;
        let by_position = LoreCompleteEventData {
            status,
            error: LoreErrorDetail::default(),
        };
        assert_eq!(by_position.status, status);
    }
}
