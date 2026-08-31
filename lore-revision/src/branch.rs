// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod create;
pub mod diff;
pub mod info;
pub mod latest;
pub mod merge;
pub mod push;
pub mod reset;

use std::cmp::PartialEq;
use std::path::PathBuf;
use std::pin::Pin;
use std::str;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use dashmap::DashMap;
use dashmap::Entry;
use futures::StreamExt;
use lore_base::lore_spawn;
use lore_base::types::BranchMetadata;
use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;
use lore_transport::Connection;
use lore_transport::MatchedProtocolError;
use lore_transport::ProtocolError;
use serde::Deserialize;
use serde::Serialize;
use tokio::join;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::wrappers::UnboundedReceiverStream;
use zerocopy::Immutable;

use crate::branch;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::commit;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::find;
use crate::hash;
use crate::immutable;
use crate::immutable::ReadFromImmutable;
use crate::immutable::WriteToImmutable;
use crate::immutable::read_options_from_repository;
use crate::interface::LoreArray;
use crate::interface::LoreBranchLocation;
use crate::interface::LoreBranchPoint;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::lore::*;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_error;
use crate::lore_info;
use crate::lore_limit_drain_tasks;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::metadata::MetadataError;
use crate::metadata::MetadataType;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeIDExt;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::Diff3Summary;
use crate::revision::DiffItem;
use crate::revision::DiffResult;
use crate::revision::sync;
use crate::state;
use crate::state::State;
use crate::state::StateData;
use crate::state::StateError;
use crate::store::KeyType;
use crate::store::MatchedStoreError;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

/// Event data reported when a branch is created.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchCreateEventData {
    /// Name of the created branch.
    pub name: LoreString,
    /// Latest revision the new branch points at.
    pub latest: Hash,
    /// Set when creating the branch also produced a new commit.
    #[serde(with = "u8_as_bool")]
    pub is_commit: u8,
}

/// Event data reported when a branch is archived.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchArchiveEventData {
    /// Name of the archived branch.
    pub name: LoreString,
}

/// Event data reported at the start of a branch listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchListBeginEventData {
    /// Location the listed branches come from.
    pub location: LoreBranchLocation,
}

/// Event data reported for each branch in a branch listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchListEntryEventData {
    /// Location this branch comes from.
    pub location: LoreBranchLocation,
    /// Branch identifier.
    pub id: BranchId,
    /// Branch name.
    pub name: LoreString,
    /// Branch category.
    pub category: LoreString,
    /// Latest revision the branch points at.
    pub latest: Hash,
    /// Stack of branch points this branch was created from.
    pub stack: LoreArray<LoreBranchPoint>,
    /// Identifier of the user who created the branch.
    pub creator: LoreString,
    /// Creation time of the branch as a timestamp.
    pub created: u64,
    /// Set when this branch is the current branch.
    #[serde(with = "u8_as_bool")]
    pub is_current: u8,
    /// Set when this branch has been archived.
    #[serde(with = "u8_as_bool")]
    pub archived: u8,
}

/// Event data reported at the end of a branch listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchListEndEventData {
    /// Location the listed branches came from.
    pub location: LoreBranchLocation,
    /// Number of branches that were listed.
    pub count: u64,
}

/// Event data reported at the start of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffBeginEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data describing a single changed node in a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffNodeData {
    /// File action applied to the node.
    pub action: LoreFileAction,
    /// Path of the node.
    pub path: LoreString,
    /// Set when the change was merged automatically.
    #[serde(with = "u8_as_bool")]
    pub automerged: u8,
    /// Previous path of the node when it was moved or copied. Empty otherwise.
    pub from_path: LoreString,
}

impl LoreBranchDiffNodeData {
    fn new(node_change: &NodeChange) -> Self {
        let is_directory_or_link = if node_change.action == FileAction::Delete {
            !node_change.from.flags.contains(NodeFlags::File)
        } else {
            !node_change.to.flags.contains(NodeFlags::File)
        };
        let display_path = |path: &str| -> LoreString {
            if is_directory_or_link {
                format!("{path}/").into()
            } else {
                path.into()
            }
        };
        Self {
            action: LoreFileAction::from(node_change.action),
            path: display_path(node_change.path.as_str()),
            automerged: node_change.flags.is_conflict_automerged().into(),
            from_path: node_change
                .from_path
                .as_ref()
                .map(|path| display_path(path.as_str()))
                .unwrap_or_default(),
        }
    }
}

/// Event data reported at the start of the change section of a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffChangeBeginEventData {
    /// Number of changes that follow.
    pub changes_count: usize,
}

/// Event data reporting a single change in a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffChangeEventData {
    /// The changed node.
    pub change: LoreBranchDiffNodeData,
}

/// Event data reported at the end of the change section of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffChangeEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported at the start of the conflict section of a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffConflictBeginEventData {
    /// Number of conflicts that follow.
    pub conflicts_count: usize,
}

/// Event data reporting a single conflict in a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffConflictEventData {
    /// The change on the source side of the conflict.
    pub source_change: LoreBranchDiffNodeData,
    /// The change on the target side of the conflict.
    pub target_change: LoreBranchDiffNodeData,
}

/// Event data reported at the end of the conflict section of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffConflictEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported at the end of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported when a branch is protected.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchProtectEventData {
    /// Name of the protected branch.
    pub name: LoreString,
}

/// Event data reported when a branch is unprotected.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchUnprotectEventData {
    /// Name of the unprotected branch.
    pub name: LoreString,
}

#[error_set]
pub enum BranchError {
    BranchNotFound,
    BranchAlreadyExists,
    DeleteProtected,
    DeleteCurrent,
    DeleteDefault,
    Divergent,
    MaxHistorySearchDepth,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    LayerNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidArguments,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    NotConnected,
    AlreadyLinked,
    BranchAdvanced,
    Conflict,
    IdenticalMetadata,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for BranchError {
    fn translated(&self) -> LoreError {
        match self {
            BranchError::Disconnected(_) => LoreError::Connection,
            BranchError::SlowDown(_) => LoreError::SlowDown,
            BranchError::Oversized(_) => LoreError::Oversized,
            BranchError::FileNotFound(_) => LoreError::FileNotFound,
            BranchError::NotFound(_)
            | BranchError::BranchNotFound(_)
            | BranchError::RevisionNotFound(_)
            | BranchError::LayerNotFound(_)
            | BranchError::LinkNotFound(_)
            | BranchError::NodeNotFound(_) => LoreError::NotFound,
            BranchError::AddressNotFound(_) => LoreError::AddressNotFound,
            BranchError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            BranchError::InvalidPath(_)
            | BranchError::InvalidArguments(_)
            | BranchError::Divergent(_) => LoreError::InvalidArguments,
            BranchError::BranchAlreadyExists(_) => LoreError::AlreadyExists,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

pub const MAX_DIVERGENT_HISTORY_LENGTH: usize = 500;

#[derive(Clone, Debug, Default, IntoBytes, FromBytes, Immutable)]
pub struct BranchLatestHistory {
    pub revision: Hash,
    pub previous: Hash,
}

// From conversions for BranchPoint and BranchMetadata are in lore-transport.

// BranchMetadata::new() is in lore-transport.

#[derive(Debug, Default, Clone)]
pub struct BranchList(pub Vec<BranchMetadata>);

pub const LATEST: &str = "branch-head";
pub const LATEST_STATUS: &str = "branch-head-status";
pub const LATEST_HISTORY: &str = "branch-head-history";
pub const LAST_SYNC: &str = "branch-last-sync";
pub const METADATA: &str = "branch-metadata";
pub const REVISION_NUMBER_STEP: &str = "branch-revision-number-step";
pub const REVISION_LIST_STEP: &str = "branch-revision-list-step";
pub const DEFAULT_HISTORY_STEP_SIZE: u64 = 100;

/// Magic identifier at the start of every cached revision-list blob.
/// Bytes spell "RLSC" (Revision-LiSt-Cache) on disk in little-endian
/// byte order, so a hex dump of the first four bytes reads
/// `52 4C 53 43` — easy to eyeball in storage tools.
pub const CACHED_REVISION_LIST_MAGIC: u32 = u32::from_le_bytes(*b"RLSC");

/// On-disk format version of the cached revision-list blob. Bump when
/// the header or item layout changes. Blobs with a different version
/// are discarded on load and rebuilt via backfill — there is no
/// in-place migration.
pub const CACHED_REVISION_LIST_VERSION: u32 = 1;

/// Fixed-size header at the start of every cached revision-list blob.
/// The remainder of the blob is a packed array of `CachedRevisionItem`.
/// 8 bytes, 4-byte aligned — fits inside the natural 8-byte alignment
/// of the items that follow.
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct CachedRevisionListHeader {
    pub magic: u32,
    pub version: u32,
}

/// Item stored in the persistent revision list cache. Mirrors the
/// `RevisionItem` proto with fixed-size fields suitable for zerocopy
/// serialization. Each cached list contains up to `step_size` of these,
/// preceded by a single [`CachedRevisionListHeader`].
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct CachedRevisionItem {
    pub number: u64,
    pub signature: Hash,
    pub metadata: Hash,
    pub state: StateData,
}

pub const NAME: &str = "name";
pub const CATEGORY: &str = "category";
pub const PARENT_DEPRECATED: &str = "parent";
pub const BRANCH_POINT_DEPRECATED: &str = "branch-point";
pub const PROTECT: &str = "protect";
pub const CREATOR: &str = "creator";
pub const CREATED: &str = "created";
pub const STACK: &str = "stack";
pub const ID: &str = "id";

pub const CATEGORY_DEFAULT: &str = "";
pub const CATEGORY_PERSONAL: &str = "personal";

pub const DEFAULT_DEFAULT_NAME: &str = "main";

fn mutable_key_type(function: &str) -> KeyType {
    match function {
        METADATA => KeyType::BranchMetadata,
        ID => KeyType::BranchId,
        LATEST => KeyType::BranchLatestPointer,
        _ => KeyType::Untyped,
    }
}

pub fn mutable_key(
    salt: &[u8],
    function: &str,
    repository: RepositoryId,
    branch: BranchId,
) -> (Hash, KeyType) {
    let key = hash::hash_function_args(
        salt,
        function,
        hex::encode(repository.data()).as_str(),
        hex::encode(branch.data()).as_str(),
    );
    let key_type = mutable_key_type(function);
    (key, key_type)
}

fn mutable_name_key(salt: &[u8], function: &str, name: &str) -> (Hash, KeyType) {
    let key = hash::hash_function_arg(salt, function, name.to_lowercase().as_str());
    let key_type = mutable_key_type(function);
    (key, key_type)
}

pub fn revision_step_key(
    salt: &[u8],
    repository: RepositoryId,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
) -> (Hash, KeyType) {
    let key_revision_number = revision_number.div_ceil(step_size) * step_size;
    let key_type = mutable_key_type(REVISION_NUMBER_STEP);
    let key = hash::hash_function_strs_slice(
        salt,
        REVISION_NUMBER_STEP,
        &[
            hex::encode(repository.data()).as_str(),
            hex::encode(branch.data()).as_str(),
            key_revision_number.to_string().as_str(),
        ],
    );
    (key, key_type)
}

/// Key for the cached revision list at the step boundary that contains
/// `revision_number`. Boundary `B = revision_number.div_ceil(step_size) * step_size`.
/// The cached list contains up to `step_size` items in segment `(B - step, B]`.
pub fn revision_list_step_key(
    salt: &[u8],
    repository: RepositoryId,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
) -> (Hash, KeyType) {
    let key_revision_number = revision_number.div_ceil(step_size) * step_size;
    let key_type = mutable_key_type(REVISION_LIST_STEP);
    let key = hash::hash_function_strs_slice(
        salt,
        REVISION_LIST_STEP,
        &[
            hex::encode(repository.data()).as_str(),
            hex::encode(branch.data()).as_str(),
            key_revision_number.to_string().as_str(),
        ],
    );
    (key, key_type)
}

fn fallback_id(name: &str) -> Context {
    let hash = Hash::hash_buffer(name.as_bytes());
    let data: [u8; 16] = hash.data()[0..16].try_into().unwrap();
    Context::from(data)
}

async fn mutable_load(
    repository: Arc<RepositoryContext>,
    function: &str,
    branch: BranchId,
) -> Result<Hash, BranchError> {
    let repository_id = repository.id;
    let (key, key_type) = mutable_key(repository.salt(), function, repository_id, branch);

    // Do not emit the error here, mutable load failures are mostly
    // benign, and in the cases where it's not it's better if the
    // call site emits the appropriate error
    let value = repository
        .read_mutable_store()
        .load(repository_id, key, key_type)
        .await
        .map_matched_err("Failed to load data from mutable store", |m| match m {
            MatchedStoreError::AddressNotFound(_) | MatchedStoreError::PayloadNotFound(_) => {
                BranchError::from(BranchNotFound {
                    branch: branch.to_string(),
                })
            }
            other => other.forward::<BranchError>("Failed to load data from mutable store"),
        })?;
    lore_debug!("Load {function} for branch {branch} repository {repository_id}: {value}");
    Ok(value)
}

async fn mutable_store(
    repository: Arc<RepositoryContext>,
    function: &str,
    branch: BranchId,
    value: Hash,
) -> Result<(), BranchError> {
    let (key, key_type) = mutable_key(repository.salt(), function, repository.id, branch);
    lore_debug!(
        "Store {function} = {value} for branch {branch} repository {}",
        repository.id
    );
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| BranchError::from(WriteRequired))?;
    handle
        .store(repository.id, key, value, key_type)
        .await
        .forward::<BranchError>("Failed to store data in mutable store")
}

pub async fn mutable_delete(
    repository: Arc<RepositoryContext>,
    function: &str,
    branch: BranchId,
) -> Result<(), BranchError> {
    let (key, key_type) = mutable_key(repository.salt(), function, repository.id, branch);
    lore_debug!(
        "Delete {function} for branch {branch} repository {}",
        repository.id
    );
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| BranchError::from(WriteRequired))?;
    handle
        .store(repository.id, key, Hash::default(), key_type)
        .await
        .forward::<BranchError>("Failed to delete data from mutable store")
}

pub async fn mutable_try_store(
    repository: Arc<RepositoryContext>,
    function: &str,
    branch: BranchId,
    expect: Hash,
    value: Hash,
) -> Result<Hash, BranchError> {
    let (key, key_type) = mutable_key(repository.salt(), function, repository.id, branch);
    lore_debug!("Store {function} = {value} for branch {branch}");

    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| BranchError::from(WriteRequired))?;
    handle
        .compare_and_swap(repository.id, key, expect, value, key_type)
        .await
        .forward::<BranchError>("Failed to compare-and-swap mutable store")
}

pub async fn store_name_to_id(
    repository: Arc<RepositoryContext>,
    id: BranchId,
    name: impl AsRef<str>,
) -> Result<(), BranchError> {
    // Store the name -> ID lookup
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name.as_ref());
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| BranchError::from(WriteRequired))?;
    handle
        .store(repository.id, key, Hash::from_context(id), key_type)
        .await
        .forward::<BranchError>("Failed to store name-to-id mapping")?;

    Ok(())
}

pub async fn delete_name_to_id(
    repository: Arc<RepositoryContext>,
    name: impl AsRef<str>,
) -> Result<(), BranchError> {
    // Delete the name -> ID lookup
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name.as_ref());
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| BranchError::from(WriteRequired))?;
    handle
        .store(repository.id, key, Hash::default(), key_type)
        .await
        .forward::<BranchError>("Failed to delete name-to-id mapping")
}

pub async fn load_name_to_id(
    repository: Arc<RepositoryContext>,
    name: impl AsRef<str>,
) -> Result<Context, BranchError> {
    let name = name.as_ref();

    if let Ok(id) = Context::from_str(name)
        && !id.is_zero()
    {
        return Ok(id);
    }

    let (key, key_type) = mutable_name_key(repository.salt(), ID, name);
    if let Ok(id) = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
    {
        return Ok(id.to_context());
    }

    if let Ok(remote) = repository.remote().await
        && let Ok(revision_service) = remote.revision(repository.id).await
        && let Ok(response) = revision_service.branch_query(None, Some(name)).await
        && !response.id.is_zero()
    {
        let branch = response.id;
        let _ = store_name_to_id(repository.clone(), branch, name).await;
        let _ = mutable_store(repository.clone(), METADATA, branch, response.metadata).await;
        return Ok(branch);
    }

    let id = fallback_id(name);
    if !id.is_zero()
        && let Ok(latest) = load_latest(repository.clone(), id).await
        && !latest.is_zero()
    {
        let _ = store_name_to_id(repository.clone(), id, name).await;
        return Ok(id);
    }

    Err(BranchError::from(BranchNotFound {
        branch: name.to_string(),
    }))
}

/// Strict local-only name-to-ID lookup. Checks only the mutable store for the
/// name-to-ID mapping. No remote query, no `fallback_id` derivation.
pub async fn load_name_to_id_local(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Context, BranchError> {
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name);
    let id = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .map_matched_err("Failed to resolve branch name", |m| match m {
            MatchedStoreError::AddressNotFound(_) | MatchedStoreError::PayloadNotFound(_) => {
                BranchError::from(BranchNotFound {
                    branch: name.to_string(),
                })
            }
            other => other.forward::<BranchError>("Failed to resolve branch name"),
        })?;
    Ok(id.to_context())
}

pub async fn load_remote(
    remote: Arc<Connection>,
    repository: RepositoryId,
    branch: BranchId,
) -> Result<BranchStatus, BranchError> {
    let revision = remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;
    let response = revision
        .branch_query(Some(branch), None)
        .await
        .map_matched_err("Failed to get information from remote", |m| match m {
            MatchedProtocolError::NotFound(_) => BranchError::from(BranchNotFound {
                branch: branch.to_string(),
            }),
            other => other.forward::<BranchError>("Failed to get information from remote"),
        })?;
    Ok(BranchStatus {
        id: branch,
        latest: response.latest,
        metadata: response.metadata,
        local: false,
        deleted: response.deleted,
    })
}

pub async fn load_latest(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Hash, BranchError> {
    if branch.is_zero() {
        return Ok(Hash::default());
    }
    match mutable_load(repository.clone(), LATEST, branch).await {
        Ok(head) => Ok(head),
        Err(err) if err.is_branch_not_found() => {
            // In case no revision is yet pushed return a zero hash
            if let Ok(_metadata) = metadata_hash(repository, branch).await {
                Ok(Hash::default())
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

pub async fn load_latest_divergent(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<bool, BranchError> {
    if branch.is_zero() {
        return Ok(false);
    }
    match mutable_load(repository.clone(), LATEST_STATUS, branch).await {
        Ok(head) => Ok(!head.is_zero()),
        Err(err) if err.is_branch_not_found() => Ok(false),
        Err(err) => Err(err),
    }
}

pub async fn load_last_sync(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Hash, BranchError> {
    if let Ok(revision) = mutable_load(repository.clone(), LAST_SYNC, branch).await {
        return Ok(revision);
    }

    if let Ok(metadata) = metadata_local(repository.clone(), branch).await
        && let Ok(branch_metadata) = branch_metadata(repository.clone(), branch, &metadata).await
        && let Some(parent) = branch_metadata.stack.first()
    {
        Ok(parent.revision)
    } else {
        Err(BranchError::from(BranchNotFound {
            branch: branch.to_string(),
        }))
    }
}

pub async fn load_remote_latest(
    remote: Arc<Connection>,
    repository: RepositoryId,
    branch: BranchId,
) -> Result<Hash, BranchError> {
    if let Ok(response) = remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?
        .branch_query(Some(branch), None)
        .await
    {
        Ok(response.latest)
    } else {
        // Silent error return, let caller determine if error
        Err(BranchError::from(BranchNotFound {
            branch: branch.to_string(),
        }))
    }
}

#[derive(PartialEq)]
pub enum BranchLatestStatus {
    /// The latest revision is not guaranteed to be in sync with remote branch history.
    /// When syncing to a new revision a divergence check has to be performed.
    Divergent,
    /// The latest revision is guaranteed to be in sync with remote branch history.
    /// When syncing to a new revision it is safe to avoid doing a divergence check.
    Convergent,
}

/// Advance a branch's local latest pointer from `previous` to `latest`.
///
/// The pointer write is a compare-and-swap against `previous`: when the stored
/// tip is anything else the branch advanced under the caller and
/// [`BranchError::BranchAdvanced`] is returned having written nothing. Creating
/// a branch passes `Hash::default()`. A caller deliberately overwriting a tip it
/// has not tracked reads it with [`load_latest`] and passes that.
pub async fn store_latest(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    previous: Hash,
    latest: Hash,
    status: BranchLatestStatus,
) -> Result<(), BranchError> {
    let stored = mutable_try_store(repository.clone(), LATEST, branch, previous, latest).await?;
    if stored != previous {
        lore_debug!(
            "Branch {branch} advanced to {stored} while storing latest {latest} (expected {previous})"
        );
        return Err(BranchAdvanced.into());
    }

    // Server does not store latest status or history
    if execution_context().is_server() {
        return Ok(());
    }

    let _ = mutable_store(
        repository.clone(),
        LATEST_STATUS,
        branch,
        if status == BranchLatestStatus::Divergent {
            latest
        } else {
            Hash::default()
        },
    )
    .await;
    store_latest_history(repository, branch, latest).await
}

pub async fn store_last_sync(repository: Arc<RepositoryContext>, branch: BranchId, revision: Hash) {
    let _ = mutable_store(repository, LAST_SYNC, branch, revision).await;
}

pub async fn load_latest_history(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    hash: Option<Hash>,
) -> Result<BranchLatestHistory, BranchError> {
    let hash = if let Some(hash) = hash {
        hash
    } else {
        mutable_load(repository.clone(), LATEST_HISTORY, branch).await?
    };

    BranchLatestHistory::read_from_immutable(
        repository.clone(),
        Address::zero_context_hash(hash),
        read_options_from_repository(&repository).no_remote(),
    )
    .await
    .forward::<BranchError>("Failed to load branch latest history")
}

pub async fn store_latest_history(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    new_latest: Hash,
) -> Result<(), BranchError> {
    // Server does not store latest history
    if execution_context().is_server() || new_latest.is_zero() {
        return Ok(());
    }

    // TODO(mjansson): Only record head pointer jumps, i.e force push operations. Otherwise
    //                 the revision list is already the head pointer list
    let old_history_latest = mutable_load(repository.clone(), LATEST_HISTORY, branch)
        .await
        .unwrap_or(Hash::default());

    // A LATEST is stored on commit and push, so this prevents adjacent duplicates in the history chain
    let old_history_latest_entry = BranchLatestHistory::read_from_immutable(
        repository.clone(),
        Address::zero_context_hash(old_history_latest),
        read_options_from_repository(&repository).no_remote(),
    )
    .await
    .unwrap_or_default();

    if old_history_latest_entry.revision == new_latest {
        return Ok(());
    }

    let entry = BranchLatestHistory {
        revision: new_latest,
        previous: old_history_latest,
    };

    let address = entry
        .write_to_immutable(
            repository.clone(),
            Context::default(),
            immutable::write_options_from_repository(repository.clone()).no_remote_write(),
        )
        .await
        .forward::<BranchError>("Failed to write branch latest history")?;

    mutable_store(repository, LATEST_HISTORY, branch, address.hash).await
}

pub async fn metadata_hash(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Hash, BranchError> {
    let result = mutable_load(repository.clone(), METADATA, branch).await;
    if let Ok(metadata) = result {
        return Ok(metadata);
    }

    if let Ok(remote) = repository.remote().await
        && let Ok(status) = load_remote(remote, repository.id, branch).await
        && !status.metadata.is_zero()
    {
        return Ok(status.metadata);
    }

    result
}

/// Store the branch metadata hash in the local mutable store cache.
pub async fn mutable_store_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    hash: Hash,
) -> Result<(), BranchError> {
    mutable_store(repository, METADATA, branch, hash).await
}

#[derive(Debug, Clone, Default)]
pub struct BranchStatus {
    /// Branch ID
    pub id: BranchId,
    /// Latest revision
    pub latest: Hash,
    /// Metadata hash
    pub metadata: Hash,
    /// Flag indicating data was local
    pub local: bool,
    /// Flag indicating branch has been deleted (name→id mapping removed)
    pub deleted: bool,
}

/// Resolve either a ID or a name given as a string to a branch metadata hash.
///
/// Honors the global `--local` / `--remote` flags: with `--local` only the
/// local mutable store is consulted (no remote calls), with `--remote` only
/// the remote is consulted, and otherwise the default behavior of preferring
/// local with remote fallback applies.
pub async fn resolve(
    repository: Arc<RepositoryContext>,
    branch: &str,
) -> Result<BranchStatus, BranchError> {
    let context = execution_context();
    let globals = context.globals();
    if globals.remote() {
        return resolve_remote(repository, branch).await;
    }
    if globals.local() {
        return resolve_local(repository, branch).await;
    }
    resolve_default(repository, branch).await
}

/// Strict local-only branch resolution. No remote calls at any step.
async fn resolve_local(
    repository: Arc<RepositoryContext>,
    branch: &str,
) -> Result<BranchStatus, BranchError> {
    // Track the name lookup result so `check_local_deleted` can reuse it
    // instead of looking up the same name again.
    let (id, name_lookup) = if let Ok(id) = Context::from_str(branch) {
        (id, None)
    } else {
        let id = load_name_to_id_local(repository.clone(), branch).await?;
        (id, Some((branch, id)))
    };

    let metadata = mutable_load(repository.clone(), METADATA, id)
        .await
        .forward::<BranchError>("loading branch metadata locally")?;
    let latest = mutable_load(repository.clone(), LATEST, id)
        .await
        .unwrap_or_default();
    let deleted = check_local_deleted(repository, id, metadata, name_lookup).await;
    Ok(BranchStatus {
        id,
        latest,
        metadata,
        local: true,
        deleted,
    })
}

/// Detects whether the local `name -> id` mapping still points at this
/// branch. `delete_name_to_id` overwrites the mapping with `Hash::default()`
/// rather than removing it, so a deleted-locally branch is observable here
/// as the mapping resolving to a different id (or to zero). Returns `false`
/// when the metadata can't be deserialized or has no name — defensive
/// defaults; the deleted bit is best-effort and reflects what we can prove.
///
/// `name_lookup` is an optional pre-computed `(name, id)` from a prior
/// `load_name_to_id_local` call — reused when the metadata's name matches,
/// to avoid hitting the mutable store twice for the same key.
async fn check_local_deleted(
    repository: Arc<RepositoryContext>,
    id: BranchId,
    metadata_hash: Hash,
    name_lookup: Option<(&str, BranchId)>,
) -> bool {
    let Ok(metadata) = load_metadata(repository.clone(), metadata_hash).await else {
        return false;
    };
    let Ok(branch_name) = name(&metadata) else {
        return false;
    };
    let mapped = match name_lookup {
        Some((cached_name, cached_id)) if cached_name == branch_name => cached_id,
        _ => match load_name_to_id_local(repository, branch_name).await {
            Ok(mapped) => mapped,
            Err(_) => return true,
        },
    };
    mapped != id
}

/// Strict remote-only branch resolution. Queries the remote by id when the
/// input parses as a `Context`, otherwise by name.
async fn resolve_remote(
    repository: Arc<RepositoryContext>,
    branch: &str,
) -> Result<BranchStatus, BranchError> {
    let branch_input = branch;
    let remote = repository.remote().await.map_err(|err| {
        BranchError::BranchNotFound(
            BranchNotFound {
                branch: branch_input.to_string(),
            }
            .chain_err_from(err, "remote unavailable for branch lookup"),
        )
    })?;
    let service = remote
        .revision(repository.id)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;
    let response = if let Ok(id) = Context::from_str(branch) {
        service.branch_query(Some(id), None).await
    } else {
        service.branch_query(None, Some(branch)).await
    };
    match response {
        Ok(response) => Ok(BranchStatus {
            id: response.id,
            latest: response.latest,
            metadata: response.metadata,
            local: false,
            deleted: response.deleted,
        }),
        Err(ProtocolError::NotFound(_)) => Err(BranchError::from(BranchNotFound {
            branch: branch_input.to_string(),
        })),
        Err(err) => Err(BranchError::internal_with_context(
            err,
            "Failed to get information from remote",
        )),
    }
}

/// Default branch resolution: prefer local, fall back to remote.
///
/// `load_name_to_id` opportunistically writes the discovered name->id and
/// metadata mappings to the local mutable store via `try_write_mutable_store`;
/// in a read-only context (e.g. `branch info`) those writes silently no-op,
/// so the cache is only populated when the caller holds a write token
/// (clone, branch switch, branch create, push, sync). Resolution itself does
/// not request a write token — it inherits whatever the caller already has.
async fn resolve_default(
    repository: Arc<RepositoryContext>,
    branch: &str,
) -> Result<BranchStatus, BranchError> {
    let branch_input = branch;
    let id = if let Ok(id) = Context::from_str(branch) {
        id
    } else if let Ok(branch) = branch::load_name_to_id(repository.clone(), branch).await {
        branch
    } else {
        let remote = repository.remote().await.map_err(|err| {
            BranchError::BranchNotFound(
                BranchNotFound {
                    branch: branch_input.to_string(),
                }
                .chain_err_from(err, "remote unavailable for branch lookup"),
            )
        })?;
        match remote
            .revision(repository.id)
            .await
            .forward::<BranchError>("Failed to connect to remote revision service")?
            .branch_query(None, Some(branch))
            .await
        {
            Ok(response) => {
                return Ok(BranchStatus {
                    id: response.id,
                    latest: response.latest,
                    metadata: response.metadata,
                    local: false,
                    deleted: response.deleted,
                });
            }
            Err(ProtocolError::NotFound(_)) => {
                return Err(BranchError::from(BranchNotFound {
                    branch: branch_input.to_string(),
                }));
            }
            Err(err) => {
                return Err(BranchError::internal_with_context(
                    err,
                    "Failed to get information from remote",
                ));
            }
        }
    };

    if let Ok(metadata) = metadata_hash(repository.clone(), id).await {
        let latest = load_latest(repository.clone(), id)
            .await
            .unwrap_or_default();
        let deleted = check_local_deleted(repository.clone(), id, metadata, None).await;
        return Ok(BranchStatus {
            id,
            latest,
            metadata,
            local: true,
            deleted,
        });
    }

    if let Ok(remote) = repository.remote().await
        && let Ok(status) = load_remote(remote, repository.id, id).await
    {
        return Ok(status);
    }

    Ok(BranchStatus {
        id,
        ..Default::default()
    })
}

pub async fn load_metadata(
    repository: Arc<RepositoryContext>,
    hash: Hash,
) -> Result<Metadata, BranchError> {
    Metadata::deserialize(repository, hash)
        .await
        .forward::<BranchError>("Failed to deserialize branch metadata")
}

pub async fn metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Metadata, BranchError> {
    let local_metadata = metadata_local(repository.clone(), branch).await;
    if local_metadata.is_ok() {
        return local_metadata;
    }

    if let Ok(remote) = repository.remote().await
        && let Ok(status) = load_remote(remote, repository.id, branch).await
    {
        mutable_store(repository.clone(), METADATA, branch, status.metadata).await?;
        return load_metadata(repository, status.metadata).await;
    }

    local_metadata
}

pub async fn metadata_local(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Metadata, BranchError> {
    let hash = metadata_hash(repository.clone(), branch).await?;
    load_metadata(repository, hash).await
}

pub async fn metadata_remote(
    remote: Arc<Connection>,
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Metadata, BranchError> {
    let status = load_remote(remote, repository.id, branch).await?;
    load_metadata(repository, status.metadata).await
}

pub fn metadata_populate(
    metadata: &mut Metadata,
    branch: BranchId,
    name: &str,
    category: &str,
    creator: &str,
    created: u64,
    stack: Vec<BranchPoint>,
) -> Result<(), BranchError> {
    metadata
        .set_binary(ID, branch.data())
        .forward::<BranchError>("Failed to populate branch ID metadata")?;
    metadata
        .set_string(NAME, name)
        .forward::<BranchError>("Failed to populate branch name metadata")?;
    metadata
        .set_string(CATEGORY, category)
        .forward::<BranchError>("Failed to populate branch category metadata")?;
    metadata
        .set_string(CREATOR, creator)
        .forward::<BranchError>("Failed to populate branch creator metadata")?;
    metadata
        .set_u64(CREATED, created)
        .forward::<BranchError>("Failed to populate branch created metadata")?;
    metadata
        .set_bool(PROTECT, false)
        .forward::<BranchError>("Failed to populate branch protect metadata")?;

    if !stack.is_empty() {
        metadata
            .set_binary(STACK, stack.as_bytes())
            .forward::<BranchError>("Failed to populate branch stack metadata")?;
    }

    // Compatibility with older clients not using the branch stack
    if let Some(parent) = stack.first() {
        let _ = metadata.set_context(PARENT_DEPRECATED, parent.branch);
        let _ = metadata.set_hash(BRANCH_POINT_DEPRECATED, parent.revision);
    }

    Ok(())
}

/// Backfill descriptive metadata fields (CATEGORY/CREATOR/CREATED) that are
/// missing from an existing branch metadata blob. Used by `branch::create`'s
/// restore paths to upgrade partial / legacy metadata so subsequent reads
/// observe a complete record. Returns `true` if at least one field was
/// written.
fn patch_missing_metadata_fields(
    metadata: &mut Metadata,
    category: &str,
    creator: &str,
    created: u64,
) -> Result<bool, BranchError> {
    let mut patched = false;
    if metadata.get_string(CATEGORY).is_err() {
        metadata
            .set_string(CATEGORY, category)
            .forward::<BranchError>("Failed to patch CATEGORY metadata")?;
        patched = true;
    }
    if metadata.get_string(CREATOR).is_err() {
        metadata
            .set_string(CREATOR, creator)
            .forward::<BranchError>("Failed to patch CREATOR metadata")?;
        patched = true;
    }
    if metadata.get_u64(CREATED).is_err() {
        metadata
            .set_u64(CREATED, created)
            .forward::<BranchError>("Failed to patch CREATED metadata")?;
        patched = true;
    }
    Ok(patched)
}

async fn metadata_store(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    metadata: Metadata,
) -> Result<Hash, BranchError> {
    let hash = metadata
        .serialize(repository.clone())
        .await
        .forward::<BranchError>("Failed to serialize branch metadata")?;

    mutable_store(repository, METADATA, branch, hash).await?;

    Ok(hash)
}

pub async fn exist_local(repository: Arc<RepositoryContext>, branch: BranchId) -> bool {
    mutable_load(repository, METADATA, branch).await.is_ok()
}

pub async fn exist_remote(
    remote: Arc<Connection>,
    repository: RepositoryId,
    branch: BranchId,
) -> bool {
    load_remote(remote, repository, branch)
        .await
        .is_ok_and(|status| !status.metadata.is_zero())
}

pub fn default_category() -> &'static str {
    CATEGORY_DEFAULT
}

pub fn personal_category() -> &'static str {
    CATEGORY_PERSONAL
}

pub fn name(metadata: &Metadata) -> Result<&str, BranchError> {
    metadata
        .get_string(NAME)
        .forward::<BranchError>("reading branch name from metadata")
}

pub fn category(metadata: &Metadata) -> Result<&str, BranchError> {
    metadata
        .get_string(CATEGORY)
        .forward::<BranchError>("reading branch category from metadata")
}

pub fn stack(metadata: &Metadata) -> Vec<BranchPoint> {
    if let Ok(stack) = metadata.get_binary(STACK) {
        return stack_from_bytes(stack);
    }

    let parent = metadata.get_context(PARENT_DEPRECATED).unwrap_or_default();
    if parent.is_zero() {
        return vec![];
    }

    let branch_point = metadata
        .get_hash(BRANCH_POINT_DEPRECATED)
        .unwrap_or_default();

    vec![BranchPoint {
        branch: parent,
        revision: branch_point,
    }]
}

#[allow(clippy::uninit_vec)]
fn stack_from_bytes(bytes: &[u8]) -> Vec<BranchPoint> {
    let count = bytes.len() / size_of::<BranchPoint>();
    let mut stack: Vec<BranchPoint> = Vec::with_capacity(count);

    // Safety: We have verified the input size as number of aligned elements,
    //         and always copy data to correctly initialize the elements in
    //         the target vector. Never writes outside of vec boundaries as
    //         number of elements is used to calculate the size to copy.
    unsafe {
        stack.set_len(count);
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            stack.as_mut_ptr().cast(),
            size_of::<BranchPoint>() * count,
        );
    }

    stack
}

pub fn creator(metadata: &Metadata) -> Result<&str, BranchError> {
    metadata
        .get_string(CREATOR)
        .forward::<BranchError>("reading branch creator from metadata")
}

pub fn created(metadata: &Metadata) -> u64 {
    metadata.get_u64(CREATED).unwrap_or_default()
}

pub async fn branch_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    metadata: &Metadata,
) -> Result<BranchMetadata, BranchError> {
    let head = load_latest(repository.clone(), branch).await?;
    let mut name = String::default();
    let mut category = String::default();
    let mut parent = Context::default();
    let mut branch_point = Hash::default();
    let mut creator = String::default();
    let mut created = 0u64;
    let mut stack = vec![];
    metadata.walk(|key, value, _value_type| {
        if key.eq(NAME.as_bytes()) {
            name = String::from_utf8_lossy(value).to_string();
        } else if key.eq(CATEGORY.as_bytes()) {
            category = String::from_utf8_lossy(value).to_string();
        } else if key.eq(PARENT_DEPRECATED.as_bytes()) {
            parent = value.into();
        } else if key.eq(BRANCH_POINT_DEPRECATED.as_bytes()) {
            branch_point = value.into();
        } else if key.eq(CREATOR.as_bytes()) {
            creator = String::from_utf8_lossy(value).to_string();
        } else if key.eq(CREATED.as_bytes()) {
            created = u64::from_le_bytes(value.try_into().unwrap_or_default());
        } else if key.eq(STACK.as_bytes()) {
            stack = stack_from_bytes(value);
        }
    });

    if stack.is_empty() && !parent.is_zero() {
        stack.push(BranchPoint {
            branch: parent,
            revision: branch_point,
        });
    }

    Ok(BranchMetadata::new(
        branch, name, category, head, creator, created, stack,
    ))
}

pub const MAX_NAME_LEN: usize = 1000;

pub fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    if let Ok(id) = Context::from_str(name)
        && !id.is_zero()
    {
        return false;
    }
    true
}

#[allow(clippy::too_many_arguments)]
pub async fn create(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    name: &str,
    category: &str,
    creator: &str,
    created: u64,
    stack: Vec<BranchPoint>,
    dry_run: bool,
    create_linked: bool,
) -> Result<Context, BranchError> {
    // Check if name is valid
    if !is_valid_name(name) {
        return Err(BranchError::internal("Invalid name"));
    }

    // Branch existence checks.
    //
    // A branch is considered to exist if it has both a name→ID mapping AND valid metadata
    // for the mapped ID. This two-part check avoids stale mappings from deleted branches.
    //
    // Step 1: Check name→ID for the given name (includes remote lookup on client).
    //   - If found and mapped ID has metadata → AlreadyExist (name taken)
    //   - If found but mapped ID has no metadata → stale mapping, ignore
    //
    // Step 2: Check ID→metadata for the given branch ID.
    //   - If no metadata → Create (fresh branch)
    //   - If metadata exists, load and check the stored name:
    //     - metadata.name == given name → Create (restore previously deleted branch,
    //       only restores the name→ID mapping, does not rewrite metadata or latest)
    //     - metadata.name != given name:
    //       - name→ID(metadata.name) exists → AlreadyExist (branch alive under old name)
    //       - name→ID(metadata.name) missing → Create (old branch fully deleted)
    //
    // Write order is metadata first, then name→ID, so that queries always see consistent
    // state (metadata exists before the name mapping points to it).
    //
    // See also: branch_query handler in urc-server which uses the same existence model
    // (server-side uses load_name_to_id_local since there is no remote to query).

    // Step 1: Check if name→ID mapping exists for the given name.
    // Uses full name lookup (including remote on client) to catch branches that
    // exist on the server but were deleted locally.
    if let Ok(mapped_id) = load_name_to_id(repository.clone(), name).await {
        // Name is taken — but only if the mapped branch still has valid metadata
        if metadata_hash(repository.clone(), mapped_id).await.is_ok() {
            lore_debug!("Branch name {name} already exists with ID {mapped_id}");
            return Err(BranchError::from(BranchAlreadyExists {
                branch: name.to_string(),
            }));
        }
        // Metadata gone for mapped ID — stale mapping, fall through to create
        lore_debug!("Stale name mapping for {name} -> {mapped_id}, metadata missing");
    }

    // Step 2: Check if ID→metadata exists for the given branch ID.
    // The metadata blob being present is the authoritative signal that the branch ID is
    // already taken — preserve LATEST/STACK and patch in whatever descriptive fields are
    // missing rather than falling through to a fresh-create overwrite path.
    if let Ok(metadata_hash_value) = metadata_hash(repository.clone(), branch).await
        && let Ok(mut existing_metadata) =
            load_metadata(repository.clone(), metadata_hash_value).await
    {
        let existing_name = branch::name(&existing_metadata).unwrap_or("");

        if !existing_name.is_empty() && existing_name != name {
            // Different name — check if the old name→ID mapping still resolves to a live branch
            if load_name_to_id(repository.clone(), existing_name)
                .await
                .is_ok()
            {
                lore_error!("Branch ID {branch} already exists under name '{existing_name}'");
                return Err(BranchError::from(BranchAlreadyExists {
                    branch: existing_name.to_string(),
                }));
            }
            lore_info!("Restoring deleted branch '{existing_name}' as '{name}' ({branch})");
        } else if existing_name.is_empty() {
            lore_info!("Restoring branch with partial metadata as '{name}' ({branch})");
        } else {
            lore_info!("Restoring deleted branch '{name}' ({branch})");
        }

        let needs_name_write = existing_name != name;
        if needs_name_write {
            existing_metadata
                .set_string(NAME, name)
                .forward::<BranchError>("Failed to update branch name metadata")?;
        }
        let patched =
            patch_missing_metadata_fields(&mut existing_metadata, category, creator, created)?;
        if needs_name_write || patched {
            metadata_store(repository.clone(), branch, existing_metadata).await?;
        }
        store_name_to_id(repository.clone(), branch, name).await?;
        return Ok(branch);
    }

    let mut head = stack
        .first()
        .map(|parent| parent.revision)
        .unwrap_or_default();

    let mut tasks = JoinSet::new();

    // Validate parent branch and revision (parallel, read-only checks)
    lore_spawn!(tasks, {
        let repository = repository.clone();
        let stack = stack.clone();
        async move {
            if let Some(parent) = stack.first() {
                if parent.branch.is_zero() {
                    lore_error!("Branch cannot have zero parent");
                    return Err(BranchError::internal("Invalid parent"));
                }
                if parent.branch == branch {
                    lore_error!("Branch cannot have itself as parent");
                    return Err(BranchError::internal("Invalid parent"));
                }

                if let Ok(parent_metadata) =
                    branch::metadata(repository.clone(), parent.branch).await
                {
                    let parent_category =
                        branch::category(&parent_metadata).unwrap_or(branch::default_category());
                    if parent_category == branch::personal_category() {
                        lore_error!("Branch cannot have a personal branch as parent");
                        return Err(BranchError::internal("Invalid parent"));
                    }
                } else {
                    lore_warn!(
                        "Could not get branch metadata to check for branch category, parent does not exist"
                    );
                }
            }

            Ok(())
        }
    });

    lore_spawn!(tasks, {
        let repository = repository.clone();
        let stack = stack.clone();
        async move {
            if let Some(parent) = stack.first() {
                if parent.revision.is_zero() {
                    let metadata_hash = repository::metadata_hash(repository.clone())
                        .await
                        .forward::<BranchError>(
                        "Failed to load repository metadata for parent validation",
                    )?;
                    let metadata = repository::metadata(repository.clone(), metadata_hash)
                        .await
                        .forward::<BranchError>(
                            "Failed to load repository metadata for parent validation",
                        )?;
                    if !parent.branch.is_zero() && parent.branch != metadata.default_branch {
                        lore_error!("Zero parent revision but parent branch is not default branch");
                        return Err(BranchError::internal("Invalid parent"));
                    }
                } else {
                    /* TODO(mjansson): Fix verifying that branch point is on the expected branch.
                                       Just a simple state load and check won't work if we're
                                       branching from the same revision as the parent branch point
                    if let Ok(state) = State::deserialize(repository.clone(), parent.revision).await
                    {
                        if state.branch(repository).await != parent.branch {
                            lore_error!("Parent revision is not on parent branch");
                            return Err(BranchError::internal("Invalid parent"));
                        }
                    } else {
                        lore_warn!(
                            "Unable to deserialize parent revision to verify branch association"
                        );
                    }
                    */
                }
            }
            Ok(())
        }
    });

    // Ensure all checks succeeded
    lore_drain_tasks!(tasks, BranchError::internal("Task failed"))?;

    let mut is_commit = false;

    if !dry_run {
        lore_debug!("Creating branch {name} {branch} with stack {stack:?} at signature {head}");

        if !head.is_zero() && create_linked {
            if let Ok((state_current, state_staged, current_branch)) =
                State::deserialize_current_and_staged(repository.clone())
                    .await
                    .forward::<BranchError>("Failed to deserialize current revision anchor")
            {
                let state = state_staged.unwrap_or(state_current);
                let serialized_latest = create_linked_branches(
                    repository.clone(),
                    token,
                    state.clone(),
                    branch,
                    current_branch,
                    head,
                    name.into(),
                    category.into(),
                )
                .await?;

                is_commit = serialized_latest != head;
                head = serialized_latest;
            }

            lore_debug!("Created linked branches, new latest {head}");
        }

        let mut metadata = Metadata::new();
        metadata_populate(
            &mut metadata,
            branch,
            name,
            category,
            creator,
            created,
            stack,
        )?;

        // Write metadata to immutable store and store ID→metadata_hash mapping
        metadata_store(repository.clone(), branch, metadata).await?;

        // Write name→ID mapping (after metadata, so queries see consistent state)
        store_name_to_id(repository.clone(), branch, name).await?;

        // Store latest revision pointer
        store_latest(
            repository.clone(),
            branch,
            Hash::default(),
            head,
            BranchLatestStatus::Divergent,
        )
        .await?;
    }

    if !head.is_zero() {
        event::LoreEvent::BranchCreate(LoreBranchCreateEventData {
            name: name.into(),
            latest: head,
            is_commit: is_commit as u8,
        })
        .send();
    }

    Ok(branch)
}

#[allow(clippy::too_many_arguments)]
async fn create_linked_branches(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: Arc<State>,
    branch: BranchId,
    current_branch: BranchId,
    current_latest: Hash,
    name: String,
    category: String,
) -> Result<Hash, BranchError> {
    let link_list = state
        .link_list(repository.clone())
        .await
        .forward::<BranchError>("Failed to list links")?;

    if link_list.is_empty() {
        return Ok(current_latest);
    }

    // Grouping keeps the cascade from racing itself: concurrent creates with the
    // same branch ID resolve to one winner, and the loser is rejected with
    // "branch has been advanced by another instance", failing the whole create.
    let link_groups = link::auto_following_mounts(&link_list);

    let mut link_tasks = JoinSet::new();

    for (link_id, mounts) in link_groups {
        lore_spawn!(link_tasks, {
            let link = Arc::new(repository.to_link_context(link_id).await);
            let link_remote = link.remote().await.forward_with::<BranchError, _>(|| {
                format!("Failed to connect to link repository {link_id}")
            })?;

            let repository = repository.clone();
            let state = state.clone();
            let branch_id = branch;
            let branch_name = name.clone();
            let branch_category = category.clone();

            async move {
                // The first mount seeds the branch point; the others adopt what
                // it resolved to, since they all address the same branch.
                let leader = mounts[0];
                let resolved_parent_branch = leader.resolve_branch(current_branch);

                let outcome = link::create_branch(
                    link.clone(),
                    link_remote,
                    branch_id,
                    branch_name,
                    branch_category,
                    resolved_parent_branch,
                    leader.signature,
                )
                .await
                .forward_with::<BranchError, _>(|| {
                    format!("Failed to create branch for link repository {link_id}")
                })?;

                // The create is shared, the reporting is not: every mount that
                // follows this repository reports its own outcome, keyed on its
                // path, because the repository ID cannot tell the mounts apart.
                for mount in mounts.iter() {
                    let link_path = state
                        .node_path(repository.clone(), mount.local_node)
                        .await
                        .unwrap_or_default();

                    link::report_branch_outcome(
                        &link_path,
                        link_id,
                        branch_id,
                        outcome.revision,
                        outcome.reused,
                    );

                    // When the link uses the implicit branch convention (zero),
                    // skip update_link_pin_by_node — the branch is already
                    // implicitly correct and the signature is unchanged (the new
                    // linked branch points to the same revision). This avoids
                    // dirtying the state and producing a bookkeeping revision.
                    if !mount.branch.is_zero() {
                        link::update_link_pin_by_node(
                            &state,
                            repository.clone(),
                            mount.repository,
                            branch_id,
                            mount.signature,
                            mount.local_node,
                        )
                        .await
                        .forward_with::<BranchError, _>(|| {
                            format!(
                                "Failed to update link reference for link repository {}",
                                mount.repository
                            )
                        })?;
                    }
                }

                Ok(())
            }
        });
    }

    lore_drain_tasks!(link_tasks, BranchError::internal("Task failed"))?;

    // If no link state was mutated (all links use implicit zero-branch
    // convention), return the current latest unchanged — no bookkeeping
    // revision needed.
    if !state.is_dirty() {
        return Ok(current_latest);
    }

    let metadata_hash = state.metadata_hash();

    if metadata_hash.is_zero() {
        return Err(BranchError::internal(
            "Failed to deserialize revision metadata",
        ));
    }

    let original_metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<BranchError>("Failed to deserialize revision metadata")?;

    let message = original_metadata
        .get_string(metadata::MESSAGE)
        .forward::<BranchError>("Failed to deserialize revision metadata")?
        .to_owned();

    let metadata = commit::prepare_commit_metadata(
        repository.clone(),
        original_metadata,
        branch,
        message.clone(),
        None,
        None,
        None,
    )
    .await
    .forward::<BranchError>("Failed setting revision metadata")?;

    state.set_parent_other(Hash::default());
    state.set_parent_self(current_latest);

    state.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .forward::<BranchError>("Failed to write revision metadata")?,
    );

    commit::weave_history(repository.clone(), state.clone())
        .await
        .forward::<BranchError>("Failed to weave history")?;

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<BranchError>("Failed to serialize revision state")?;

    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<BranchError>("Failed to serialize anchor")?;

    Ok(signature)
}

pub async fn delete(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<(), BranchError> {
    let mut branch_name = String::default();

    // Do not allow deleting the current branch
    if let Ok((_revision, current_branch)) = crate::instance::load_current_anchor(&repository).await
        && current_branch == branch
    {
        return Err(BranchError::from(DeleteCurrent {
            branch: branch.to_string(),
        }));
    }

    if let Ok(branch_metadata) = metadata(repository.clone(), branch).await {
        branch_name = name(&branch_metadata).unwrap_or_default().to_string();
        let display = if branch_name.is_empty() {
            branch.to_string()
        } else {
            branch_name.clone()
        };

        // Check if protected
        if branch_metadata.get_bool(PROTECT).unwrap_or_default() {
            return Err(BranchError::from(DeleteProtected { branch: display }));
        }

        // Do not allow deleting the default branch
        if let Ok(repository_metadata) = repository::metadata_hash(repository.clone()).await
            && let Ok(repository_metadata) =
                repository::metadata(repository.clone(), repository_metadata).await
            && repository_metadata.default_branch == branch
        {
            return Err(BranchError::from(DeleteDefault { branch: display }));
        }

        // Old default branch check, can be removed eventually
        let stack = stack(&branch_metadata);
        if stack.is_empty() {
            return Err(BranchError::from(DeleteDefault { branch: display }));
        }
    }

    // Check if the branch exist or has been deleted
    if branch_name.is_empty() {
        return Err(BranchError::from(BranchNotFound {
            branch: branch.to_string(),
        }));
    }

    // If the name now points to another branch it means this branch has been deleted
    if load_name_to_id(repository.clone(), &branch_name).await? != branch {
        return Err(BranchError::from(BranchNotFound {
            branch: branch_name,
        }));
    }

    delete_name_to_id(repository.clone(), &branch_name).await?;

    // A layer or link archive is a consequence of the outer one, not its own
    // user-facing event, so reporting each one would read as repeated archives.
    if !repository.is_layer() && !repository.is_link() {
        event::LoreEvent::BranchArchive(LoreBranchArchiveEventData {
            name: branch_name.into(),
        })
        .send();
    }

    Ok(())
}

pub async fn delete_remote(
    remote: Arc<Connection>,
    repository: RepositoryId,
    branch: BranchId,
) -> Result<(), BranchError> {
    let remote = remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;

    // The service answers NOT_FOUND here for one reason only, and callers
    // distinguish the missing-branch case to decide whether to skip. Left as the
    // transport's generic NotFound it is indistinguishable from a real failure.
    match remote.branch_delete(branch).await {
        Err(err) if err.is_not_found() => Err(BranchError::BranchNotFound(
            BranchNotFound {
                branch: branch.to_string(),
            }
            .chain_err_from(err, "branch missing on remote"),
        )),
        result => result.forward::<BranchError>("Failed to delete branch on remote"),
    }
}

pub async fn protect(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<(), BranchError> {
    set_protect(repository, branch, true).await?;
    Ok(())
}

pub async fn unprotect(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<(), BranchError> {
    set_protect(repository, branch, false).await?;
    Ok(())
}

// Toggle PROTECT on the branch metadata. v1 deprecated the dedicated protect/unprotect RPCs — the bit lives on the metadata blob and the server lets BranchMetadataSet write it directly. When no remote is configured (local-only repository, server-side context) only the local cache is updated
async fn set_protect(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    value: bool,
) -> Result<(), BranchError> {
    if repository.remote().await.is_ok() {
        crate::metadata::branch::set(
            repository.clone(),
            branch,
            &[PROTECT.as_bytes()],
            &[if value { &[1u8] } else { &[0u8] }],
            &[crate::metadata::MetadataType::Boolean],
        )
        .await
        .forward_with::<BranchError, _>(|| {
            if value {
                "Failed to protect branch on remote".to_string()
            } else {
                "Failed to unprotect branch on remote".to_string()
            }
        })?;
    } else {
        let mut branch_metadata = metadata(repository.clone(), branch).await?;
        branch_metadata
            .set_bool(PROTECT, value)
            .forward::<BranchError>("Failed to update branch protect metadata")?;
        metadata_store(repository.clone(), branch, branch_metadata).await?;
    }

    let metadata_hash = metadata_hash(repository.clone(), branch).await?;
    let branch_metadata = load_metadata(repository, metadata_hash)
        .await
        .forward::<BranchError>("Failed to load branch metadata")?;

    if value {
        event::LoreEvent::BranchProtect(LoreBranchProtectEventData {
            name: name(&branch_metadata)?.into(),
        })
        .send();
    } else {
        event::LoreEvent::BranchUnprotect(LoreBranchUnprotectEventData {
            name: name(&branch_metadata)?.into(),
        })
        .send();
    }

    Ok(())
}

pub async fn list(
    repository: Arc<RepositoryContext>,
) -> Result<impl tokio_stream::Stream<Item = Context>, BranchError> {
    let stream = repository
        .read_mutable_store()
        .list(repository.id, KeyType::BranchId)
        .await
        .forward::<BranchError>("Failed to list branches from store")?;

    Ok(UnboundedReceiverStream::new(stream.channel()).map(|(_, id)| id.to_context()))
}

pub async fn list_remote(
    remote: Arc<Connection>,
    repository: RepositoryId,
) -> Result<Vec<BranchMetadata>, BranchError> {
    let remote = remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;

    let response = remote
        .branch_list()
        .await
        .forward::<BranchError>("Failed to list branches on remote")?;

    Ok(response.list)
}

pub async fn list_output(
    repository: Arc<RepositoryContext>,
    local: bool,
    remote: bool,
    archived: bool,
) -> Result<(), BranchError> {
    if remote {
        return list_remote_output(repository, true).await;
    }

    // List local branches
    event::LoreEvent::BranchListBegin(LoreBranchListBeginEventData {
        location: LoreBranchLocation::Local,
    })
    .send();

    let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<BranchError>("Failed to deserialize current revision anchor")?;

    let active_ids = Arc::new(dashmap::DashSet::<BranchId>::new());
    let count = Arc::new(AtomicUsize::new(0));
    const MAX_TASKS: usize = 100;
    let mut tasks = JoinSet::new();
    let mut metadata_stream = list(repository.clone()).await?;
    while let Some(id) = metadata_stream.next().await {
        let repository = repository.clone();
        let count = count.clone();
        let active_ids = active_ids.clone();
        lore_spawn!(tasks, async move {
            if archived {
                active_ids.insert(id);
            }

            let metadata_hash = metadata_hash(repository.clone(), id).await?;
            let metadata = load_metadata(repository.clone(), metadata_hash).await?;

            let name = branch::name(&metadata)?;
            let category = branch::category(&metadata).unwrap_or(branch::default_category());
            let latest = branch::load_latest(repository.clone(), id)
                .await
                .unwrap_or_default();
            let stack = branch::stack(&metadata);
            let creator = branch::creator(&metadata)?;
            let created = branch::created(&metadata);

            event::LoreEvent::BranchListEntry(LoreBranchListEntryEventData {
                location: LoreBranchLocation::Local,
                id,
                name: name.into(),
                category: category.into(),
                latest,
                stack: LoreArray::<LoreBranchPoint>::from_vec(
                    stack
                        .iter()
                        .map(|parent| LoreBranchPoint {
                            branch: parent.branch,
                            revision: parent.revision,
                        })
                        .collect(),
                ),
                creator: creator.into(),
                created,
                is_current: (id == current_branch) as u8,
                archived: 0,
            })
            .send();

            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            Ok(())
        });

        let _ = lore_limit_drain_tasks!(tasks, MAX_TASKS, BranchError::internal("Task failed"));
    }

    let _ = lore_drain_tasks!(tasks, BranchError::internal("Task failed"));

    event::LoreEvent::BranchListEnd(LoreBranchListEndEventData {
        location: LoreBranchLocation::Local,
        count: count.load(std::sync::atomic::Ordering::Relaxed) as u64,
    })
    .send();

    // List archived local branches
    if archived {
        event::LoreEvent::BranchListBegin(LoreBranchListBeginEventData {
            location: LoreBranchLocation::Local,
        })
        .send();

        let all_metadata_stream = repository
            .read_mutable_store()
            .list(repository.id, KeyType::BranchMetadata)
            .await
            .forward::<BranchError>("Failed to list branch metadata from store")?;

        let archived_count = Arc::new(AtomicUsize::new(0));
        let mut archived_tasks = JoinSet::new();
        let mut all_metadata = UnboundedReceiverStream::new(all_metadata_stream.channel());
        while let Some((_key, value)) = all_metadata.next().await {
            let repository = repository.clone();
            let archived_count = archived_count.clone();
            let active_ids = active_ids.clone();
            lore_spawn!(archived_tasks, async move {
                let metadata = load_metadata(repository.clone(), value).await;
                let Ok(metadata) = metadata else {
                    return Ok(());
                };

                let Ok(id_bytes) = metadata.get_binary(ID) else {
                    return Ok(());
                };
                let id: BranchId = id_bytes.into();

                if active_ids.contains(&id) {
                    return Ok(());
                }

                let name = branch::name(&metadata)?;
                let category = branch::category(&metadata).unwrap_or(branch::default_category());
                let stack = branch::stack(&metadata);
                let creator = branch::creator(&metadata)?;
                let created = branch::created(&metadata);

                event::LoreEvent::BranchListEntry(LoreBranchListEntryEventData {
                    location: LoreBranchLocation::Local,
                    id,
                    name: name.into(),
                    category: category.into(),
                    latest: Hash::default(),
                    stack: LoreArray::<LoreBranchPoint>::from_vec(
                        stack
                            .iter()
                            .map(|parent| LoreBranchPoint {
                                branch: parent.branch,
                                revision: parent.revision,
                            })
                            .collect(),
                    ),
                    creator: creator.into(),
                    created,
                    is_current: 0,
                    archived: 1,
                })
                .send();

                archived_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                Ok(())
            });

            let _ = lore_limit_drain_tasks!(
                archived_tasks,
                MAX_TASKS,
                BranchError::internal("Task failed")
            );
        }

        let _ = lore_drain_tasks!(archived_tasks, BranchError::internal("Task failed"));

        event::LoreEvent::BranchListEnd(LoreBranchListEndEventData {
            location: LoreBranchLocation::Local,
            count: archived_count.load(std::sync::atomic::Ordering::Relaxed) as u64,
        })
        .send();
    }

    if local {
        return Ok(());
    }

    list_remote_output(repository, false).await
}

/// Emit the remote branch list. When `required` (an explicit `--remote`), a
/// missing connection or a failed remote listing is propagated as an error;
/// otherwise the remote is optional and such failures emit no remote events.
async fn list_remote_output(
    repository: Arc<RepositoryContext>,
    required: bool,
) -> Result<(), BranchError> {
    let remote = match repository.remote().await {
        Ok(remote) => remote,
        Err(_) if !required => return Ok(()),
        connection => connection.forward::<BranchError>("Failed to connect to remote")?,
    };

    let list = match list_remote(remote, repository.id).await {
        Ok(list) => list,
        Err(err) if required => return Err(err),
        Err(_) => return Ok(()),
    };

    event::LoreEvent::BranchListBegin(LoreBranchListBeginEventData {
        location: LoreBranchLocation::Remote,
    })
    .send();

    for entry in &list {
        let id = entry.id;
        let name = &entry.name;
        let category = &entry.category;
        let latest = entry.latest;
        let creator = &entry.creator;
        let created = entry.created;
        let stack = &entry.stack;

        event::LoreEvent::BranchListEntry(LoreBranchListEntryEventData {
            location: LoreBranchLocation::Remote,
            id,
            name: name.into(),
            category: category.into(),
            latest,
            creator: creator.into(),
            created,
            stack: LoreArray::<LoreBranchPoint>::from_vec(
                stack.iter().map(LoreBranchPoint::from).collect(),
            ),
            is_current: 0,
            archived: 0,
        })
        .send();
    }

    event::LoreEvent::BranchListEnd(LoreBranchListEndEventData {
        location: LoreBranchLocation::Remote,
        count: list.len() as u64,
    })
    .send();

    Ok(())
}

#[derive(Debug)]
pub struct RevisionListItem {
    pub revision: Hash,
    pub revision_number: u64,
    pub parent_self: Hash,
    pub parent_other: Hash,
    pub parent_self_revision_number: Option<u64>,
    pub parent_other_revision_number: Option<u64>,
    pub metadata: Metadata,
}

impl From<&RevisionListItem> for lore_proto::Revision {
    fn from(revision: &RevisionListItem) -> Self {
        let mut proto_revision = lore_proto::Revision {
            id: revision.revision.into(),
            commit_message: String::default(),
            timestamp: 0,
            created_by: String::default(),
            committed_by: String::default(),
            metadata: Vec::default(),
            parent_self: if revision.parent_self.is_zero() {
                None
            } else {
                Some(revision.parent_self.into())
            },
            parent_other: if revision.parent_other.is_zero() {
                None
            } else {
                Some(revision.parent_other.into())
            },
            number: revision.revision_number,
            parent_self_number: revision.parent_self_revision_number,
            parent_other_number: revision.parent_other_revision_number,
        };
        revision.metadata.walk(|key, value, value_type| {
            let key = std::str::from_utf8(key).unwrap_or("<binary>");
            match key {
                metadata::MESSAGE => {
                    proto_revision.commit_message =
                        std::str::from_utf8(value).unwrap_or("<binary>").to_string();
                }
                metadata::TIMESTAMP => {
                    if value.len() == std::mem::size_of::<u64>() {
                        proto_revision.timestamp = u64::from_le_bytes(value.try_into().unwrap());
                    }
                }
                metadata::CREATED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        proto_revision.created_by = value.to_string();
                    }
                }
                metadata::COMMITTED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        proto_revision.committed_by = value.to_string();
                    }
                }
                _ => {
                    let metadata =
                        as_lore_proto_metadata(String::from(key), value, value_type).ok();
                    if let Some(metadata) = metadata {
                        proto_revision.metadata.push(metadata);
                    }
                }
            }
        });

        proto_revision
    }
}

fn as_lore_proto_metadata(
    key: String,
    value: &[u8],
    value_type: MetadataType,
) -> Result<lore_proto::Metadata, MetadataError> {
    let metadata_type = match value_type {
        MetadataType::Address => lore_proto::MetadataType::Address,
        MetadataType::Boolean => lore_proto::MetadataType::Boolean,
        MetadataType::Context => lore_proto::MetadataType::Context,
        MetadataType::Hash => lore_proto::MetadataType::Hash,
        MetadataType::Numeric => lore_proto::MetadataType::Numeric,
        MetadataType::String => lore_proto::MetadataType::String,
        MetadataType::Binary => lore_proto::MetadataType::Binary,
    };
    let value = match value_type {
        MetadataType::Address => Metadata::to_address(value).map(|val| format!("{val}"))?,
        MetadataType::Boolean => Metadata::to_bool(value).map(|val| format!("{val}"))?,
        MetadataType::Context => Metadata::to_context(value).map(|val| format!("{val}"))?,
        MetadataType::Hash => Metadata::to_hash(value).map(|val| format!("{val}"))?,
        MetadataType::Numeric => Metadata::to_u64(value).map(|val| format!("{val}"))?,
        MetadataType::String => Metadata::to_string(value).map(|val| val.to_string())?,
        MetadataType::Binary => format!("<Binary, {} bytes>", value.len()),
    };

    Ok(lore_proto::Metadata {
        key,
        value,
        metadata_type: metadata_type.into(),
    })
}

pub struct RevisionListResult {
    pub revisions: Vec<RevisionListItem>,
    pub has_more: bool,
}

/// Calculate the list of revisions from latest up to the branch point.
///
/// When both `source` and `target` are provided, `branch` may be `None`. When `source` is
/// provided without `target`, the branch is derived from the source revision so `branch` may
/// also be `None`. When `source` is absent, `branch` must be `Some` so the latest revision
/// can be resolved.
pub async fn list_revisions(
    repository: Arc<RepositoryContext>,
    branch: Option<Context>,
    limit: Option<usize>,
    source: Option<Hash>,
    target: Option<Hash>,
) -> Result<RevisionListResult, BranchError> {
    let start_revision_id = if let Some(s) = source {
        s
    } else {
        let b = branch.ok_or_else(|| {
            BranchError::from(InvalidArguments {
                reason: "branch argument is required when source is not provided".into(),
            })
        })?;
        load_latest(repository.clone(), b).await?
    };
    let final_revision_id = if let Some(t) = target {
        t
    } else {
        let b = if let Some(b) = branch {
            b
        } else {
            let state = State::deserialize(repository.clone(), start_revision_id)
                .await
                .forward::<BranchError>("Failed to deserialize revisions state")?;
            state.branch(repository.clone()).await
        };
        let branch_metadata = metadata(repository.clone(), b).await?;
        stack(&branch_metadata)
            .first()
            .map(|parent| parent.revision)
            .unwrap_or_default()
    };
    let limit = limit.unwrap_or(100);

    let mut walk_count: usize = 1;
    let mut result = RevisionListResult {
        revisions: vec![],
        has_more: false,
    };

    // Loop from source (defaults at latest) until the branch point (or if we hit the limit)
    let mut current_id = start_revision_id;
    lore_debug!("Looping from {} to {}", current_id, final_revision_id);
    while current_id != final_revision_id && !current_id.is_zero() && walk_count <= limit {
        let current_state = State::deserialize(repository.clone(), current_id)
            .await
            .forward::<BranchError>("Failed to deserialize revision state")?;

        let metadata = Metadata::deserialize(repository.clone(), current_state.metadata_hash())
            .await
            .forward::<BranchError>("Failed to deserialize revision metadata")?;

        lore_trace!(
            "current rev {current_id} final {final_revision_id} walk_count {walk_count} limit {limit}"
        );
        let parent_other_revision_number = if !current_state.parent_other().is_zero() {
            let parent_other_state =
                State::deserialize(repository.clone(), current_state.parent_other())
                    .await
                    .forward::<BranchError>("Failed to deserialize parent_other state")?;
            Some(parent_other_state.revision_number())
        } else {
            None
        };

        // The previous item's parent_self is this item, so fill in its revision number.
        if let Some(prev) = result.revisions.last_mut() {
            prev.parent_self_revision_number = Some(current_state.revision_number());
        }

        result.revisions.push(RevisionListItem {
            revision: current_id,
            revision_number: current_state.revision_number(),
            parent_self: current_state.parent_self(),
            parent_other: current_state.parent_other(),
            parent_self_revision_number: None,
            parent_other_revision_number,
            metadata,
        });
        current_id = current_state.parent_self();
        walk_count += 1;
    }

    // The last item's parent_self may point outside this page; resolve it if non-zero.
    if let Some(last) = result.revisions.last_mut()
        && !last.parent_self.is_zero()
    {
        let parent_state = State::deserialize(repository.clone(), last.parent_self)
            .await
            .forward::<BranchError>("Failed to deserialize parent_self state")?;
        last.parent_self_revision_number = Some(parent_state.revision_number());
    }

    if current_id != final_revision_id && !current_id.is_zero() {
        result.has_more = true;
    }

    Ok(result)
}

/// Streaming 3-way branch diff over `revision::diff3`. When
/// `auto_resolve` is set, each `DiffItem::Conflict` is re-emitted as
/// `DiffItem::Change` if the per-conflict text-merge succeeds —
/// processed inline rather than buffered, so memory stays bounded at
/// one conflict's worth of file realisation regardless of total
/// conflict count.
#[allow(clippy::too_many_arguments)]
pub async fn diff3(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    auto_resolve: bool,
    graft_view: Option<Arc<crate::filter::Filter>>,
    tx: mpsc::Sender<Result<DiffItem, BranchError>>,
) -> Result<Diff3Summary, BranchError> {
    Box::pin(diff3_with_source_cap(
        repository,
        source_branch,
        source_revision,
        target_branch,
        target_revision,
        path,
        include_same,
        auto_resolve,
        None,
        None,
        graft_view,
        tx,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn diff3_with_source_cap(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    auto_resolve: bool,
    source_cap: Option<usize>,
    history_walk_concurrency: Option<usize>,
    graft_view: Option<Arc<crate::filter::Filter>>,
    tx: mpsc::Sender<Result<DiffItem, BranchError>>,
) -> Result<Diff3Summary, BranchError> {
    lore_info!(
        "Branch diff branch {source_branch} revision {source_revision} -> branch {target_branch} revision {target_revision}"
    );

    let base_revision = resolve_diff3_base(
        repository.clone(),
        source_branch,
        source_revision,
        target_branch,
        target_revision,
    )
    .await?;

    // `resolve_diff3_base` is documented never to return this, so reaching it is a
    // bug rather than a repository state. Refuse anyway: the alternative is a diff
    // against the empty tree that conflicts on every path in both branches.
    if base_revision.is_zero() {
        lore_error!(
            "Resolved a zero base revision for branch {source_branch} revision {source_revision} and branch {target_branch} revision {target_revision}"
        );
        return Err(BranchError::from(Divergent));
    }

    lore_info!(
        "Revision diff base {base_revision} source {source_revision} target {target_revision}"
    );

    let summary = Diff3Summary {
        base: base_revision,
        source: source_revision,
        target: target_revision,
    };

    let (inner_tx, mut inner_rx) = mpsc::channel::<Result<DiffItem, StateError>>(256);
    let mut driver = std::pin::pin!(revision::diff3_with_source_cap(
        repository.clone(),
        base_revision,
        source_revision,
        target_revision,
        path,
        include_same,
        source_cap,
        history_walk_concurrency,
        graft_view,
        inner_tx,
    ));
    loop {
        tokio::select! {
            biased;
            item = inner_rx.recv() => if let Some(item) = item {
                let item = item.forward::<BranchError>("Failed to calculate branch diff")?;
                emit_diff_item_with_auto_resolve(item, auto_resolve, &tx).await?;
            } else {
                (&mut driver).await.forward::<BranchError>("Failed to calculate branch diff")?;
                break;
            },
            result = &mut driver => {
                result.forward::<BranchError>("Failed to calculate branch diff")?;
                while let Some(item) = inner_rx.recv().await {
                    let item = item.forward::<BranchError>("Failed to calculate branch diff")?;
                    emit_diff_item_with_auto_resolve(item, auto_resolve, &tx).await?;
                }
                break;
            }
        }
    }

    Ok(summary)
}

/// Per-`DiffItem` step of `branch::diff3`'s auto-resolve drain. Kept
/// sequential: parallelising without a concurrency cap breaks the
/// streaming pipeline's memory bound — each in-flight conflict pins
/// two `NodeChange`s and three open temp files until the text-merge
/// completes.
async fn emit_diff_item_with_auto_resolve(
    item: DiffItem,
    auto_resolve: bool,
    tx: &mpsc::Sender<Result<DiffItem, BranchError>>,
) -> Result<(), BranchError> {
    match item {
        DiffItem::Change(c) => tx
            .send(Ok(DiffItem::Change(c)))
            .await
            .map_err(|_send_err| Internal::msg("diff3 channel closed").into()),
        DiffItem::Conflict(pair) => {
            let (change_from, change_to) = *pair;
            if auto_resolve
                && let Some(resolved) = try_auto_resolve_conflict(&change_from, &change_to).await?
            {
                return tx
                    .send(Ok(DiffItem::Change(resolved)))
                    .await
                    .map_err(|_send_err| Internal::msg("diff3 channel closed").into());
            }
            tx.send(Ok(DiffItem::Conflict(Box::new((change_from, change_to)))))
                .await
                .map_err(|_send_err| Internal::msg("diff3 channel closed").into())
        }
    }
}

/// Realises the three sides of one conflict into temp files and runs
/// `merge3_text_by_pathbuf`. Returns `Some(resolved_change)` only when
/// the merge produces no conflict markers — any merge failure or any
/// markers in the output preserve the conflict (returns `None`).
async fn try_auto_resolve_conflict(
    change_from: &NodeChange,
    change_to: &NodeChange,
) -> Result<Option<NodeChange>, BranchError> {
    if change_from.path != change_to.path {
        return Ok(None);
    }
    let theirs_path: PathBuf = util::fs::generate_temppath("theirs");
    let base_path: PathBuf = util::fs::generate_temppath("base");
    let mine_path: PathBuf = util::fs::generate_temppath("mine");
    let theirs_file = theirs_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let base_file = base_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mine_file = mine_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    if change_from.to.node.is_valid_node_id() {
        lore_trace!("Change from theirs has valid to node, realize theirs file {theirs_file}");
        let node = change_from
            .to
            .state
            .block(
                change_from.to.repository.clone(),
                NodeBlock::index(change_from.to.node),
            )
            .await
            .forward::<BranchError>("Failed to deserialize revisions state")?
            .node(Node::index(change_from.to.node));
        // TODO(vri): UCS-19228 - Links: Realize link node files during branch sync
        if node.is_file() {
            if sync::realize_scratch_file(
                change_from.to.repository.clone(),
                &theirs_path,
                node,
                Arc::default(),
            )
            .await
            .forward::<BranchError>("Failed to auto resolve file")
            .is_err()
            {
                return Ok(None);
            }
        } else {
            lore_trace!("Change from theirs is not a file, ignore auto resolve");
            return Ok(None);
        }
    } else {
        lore_trace!("Change from theirs has no valid to node, empty theirs file");
        let _ = lore_io::IoDriver::global()
            .write_file_bytes(&theirs_path, bytes::Bytes::new(), false)
            .await;
    }

    if !crate::infer::infer_is_diffable_by_path(&theirs_path)
        .await
        .unwrap_or(false)
    {
        lore_trace!("Change is not diffable and cannot be auto resolved, continue");
        return Ok(None);
    }

    if change_from.from.node.is_valid_node_id() {
        lore_trace!("Change from base has valid from node, realize base file {base_file}");
        let node = change_from
            .from
            .state
            .block(
                change_from.from.repository.clone(),
                NodeBlock::index(change_from.from.node),
            )
            .await
            .forward::<BranchError>("Failed to deserialize revisions state")?
            .node(Node::index(change_from.from.node));
        // TODO(vri): UCS-19228 - Links: Realize link node files during branch sync
        if node.is_file() {
            if sync::realize_scratch_file(
                change_from.from.repository.clone(),
                &base_path,
                node,
                Arc::default(),
            )
            .await
            .forward::<BranchError>("Failed to auto resolve file")
            .is_err()
            {
                return Ok(None);
            }
        } else {
            lore_trace!("Change from base is not a file, ignore auto resolve");
            return Ok(None);
        }
    } else {
        lore_trace!("Change from base has no valid to node, empty base file");
        let _ = lore_io::IoDriver::global()
            .write_file_bytes(&base_path, bytes::Bytes::new(), false)
            .await;
    }

    if change_to.to.node.is_valid_node_id() {
        lore_trace!("Change to mine has valid from node, realize mine file {mine_file}");
        let node = change_to
            .to
            .state
            .block(
                change_to.to.repository.clone(),
                NodeBlock::index(change_to.to.node),
            )
            .await
            .forward::<BranchError>("Failed to deserialize revisions state")?
            .node(Node::index(change_to.to.node));
        // TODO(vri): UCS-19228 - Links: Realize link node files during branch sync
        if node.is_file() {
            if sync::realize_scratch_file(
                change_to.to.repository.clone(),
                &mine_path,
                node,
                Arc::default(),
            )
            .await
            .forward::<BranchError>("Failed to auto resolve file")
            .is_err()
            {
                return Ok(None);
            }
        } else {
            lore_trace!("Change to mine is not a file, ignore auto resolve");
            return Ok(None);
        }
    } else {
        lore_trace!("Change to mine has no valid to node, empty mine file");
        let _ = lore_io::IoDriver::global()
            .write_file_bytes(&mine_path, bytes::Bytes::new(), false)
            .await;
    }

    let resolved = match crate::merge::merge3_text_by_pathbuf(
        base_path.clone(),
        mine_path.clone(),
        theirs_path.clone(),
        mine_path.clone(),
        crate::merge::MergeTextMode::DryRun,
    )
    .await
    {
        Err(err) => {
            lore_debug!(
                "Auto resolve failed for base {base_file}, mine {mine_file}, theirs {theirs_file} - conflict remains: {err}"
            );
            false
        }
        Ok(true) => {
            lore_debug!(
                "Auto resolved with conflict markers for base {base_file}, mine {mine_file}, theirs {theirs_file} - conflict remains"
            );
            false
        }
        Ok(false) => {
            lore_trace!(
                "Auto resolved without any line conflicts for base {base_file}, mine {mine_file}, theirs {theirs_file} - conflict resolved"
            );
            true
        }
    };

    let _ = util::fs::unlink(base_path).await;
    let _ = util::fs::unlink(theirs_path).await;
    let _ = util::fs::unlink(mine_path).await;

    if resolved {
        Ok(Some(NodeChange {
            action: change_to.action,
            flags: change_to.flags | change::Flags::ConflictAutomerged,
            from: change_to.from.clone(),
            to: change_to.to.clone(),
            path: change_to.path.clone(),
            from_path: change_to.from_path.clone(),
        }))
    } else {
        Ok(None)
    }
}

/// Resolve the common-ancestor base revision for a 3-way diff between two
/// branches' tips. Performs the branch-stack walking, branch-point
/// matching, and divergence detection that `diff3_collect` would otherwise
/// run internally — exposed as a standalone function so callers that need
/// to know the base **before** consuming the diff stream (e.g. the v1
/// `RevisionDiff` gRPC handler, which carries the resolved base in its
/// response header) can compute it up front and pass it into
/// `diff3_streaming` without duplicating the work.
///
/// Two branches resolve in two steps.
/// `find_common_ancestor_from_branch_points` reads the stacks for the branch
/// both sides descend from and the revision on it each side branched at, and
/// answers from those. `find_common_ancestor_from_merges` then improves on that
/// answer where a merge has already carried one branch into the other, which the
/// branch points cannot show.
///
/// Exhausting the searches is not fatal. Both branch points sit on the branch the
/// stacks share, so the older of the two is always available as an answer, and it
/// is used - with a warning - rather than refusing a diff the caller has no way
/// to repair. Stacks that share no branch at all are fatal, with
/// `BranchError::InvalidArguments`: every branch descends from the default
/// branch, so that is an invalid branch configuration rather than a history the
/// search failed on.
///
/// A branch resolved against itself has no branch points to fall back on, and
/// fails with `BranchError::Divergent` when its two revisions share no history.
///
/// Never returns the zero revision.
pub async fn resolve_diff3_base(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
) -> Result<Hash, BranchError> {
    lore_debug!(
        "Resolve diff base between source branch {source_branch} revision {source_revision} and target branch {target_branch} revision {target_revision}"
    );

    let common_ancestor = if source_branch != target_branch {
        let source_stack = if let Ok(branch_metadata) =
            metadata(repository.clone(), source_branch).await
        {
            let stack = stack(&branch_metadata);
            lore_debug!(
                "Loaded local metadata for source branch {source_branch}, found stack {stack:?}"
            );
            stack
        } else if let Ok(remote) = repository.remote().await {
            if let Ok(branch_metadata) =
                metadata_remote(remote, repository.clone(), source_branch).await
            {
                let stack = stack(&branch_metadata);
                lore_debug!(
                    "Loaded remote metadata for source branch {source_branch}, found stack {stack:?}"
                );
                stack
            } else {
                lore_debug!("No local or remote source branch metadata available");
                vec![]
            }
        } else {
            lore_debug!("No local source branch metadata available and have no remote");
            vec![]
        };

        let target_stack = if let Ok(branch_metadata) =
            metadata(repository.clone(), target_branch).await
        {
            let stack = stack(&branch_metadata);
            lore_debug!(
                "Loaded local metadata for target branch {target_branch}, found stack {stack:?}"
            );
            stack
        } else if let Ok(remote) = repository.remote().await {
            if let Ok(branch_metadata) =
                metadata_remote(remote, repository.clone(), target_branch).await
            {
                let stack = stack(&branch_metadata);
                lore_debug!(
                    "Loaded remote metadata for target branch {target_branch}, found stack {stack:?}"
                );
                stack
            } else {
                lore_debug!("No local or remote target branch metadata available");
                vec![]
            }
        } else {
            lore_debug!("No local target branch metadata available and have no remote");
            vec![]
        };

        let Some(common_ancestor) = Box::pin(find_common_ancestor_from_branch_points(
            repository.clone(),
            source_branch,
            source_revision,
            &source_stack,
            target_branch,
            target_revision,
            &target_stack,
        ))
        .await?
        else {
            return Err(InvalidArguments {
                reason: format!(
                    "source branch {source_branch} and target branch {target_branch} have no common branch in their branch stacks"
                ),
            }
            .into());
        };

        lore_debug!("Branch points give common ancestor {common_ancestor}");

        common_ancestor
    } else {
        let branch_point = metadata(repository.clone(), source_branch)
            .await
            .map(|metadata| {
                let stack = stack(&metadata);
                lore_debug!(
                    "Loaded local metadata for source branch {source_branch}, found stack {stack:?}"
                );
                stack.first().map(|parent| parent.revision)
            })
            .unwrap_or_default();
        let common_ancestor = Box::pin(find_common_revision_in_history_lines(
            repository.clone(),
            source_revision,
            target_revision,
        ))
        .await?
        .or(branch_point)
        .unwrap_or_default();

        lore_debug!("History lines give common ancestor {common_ancestor}");

        common_ancestor
    };

    let base_revision = find_common_ancestor_from_merges(
        repository.clone(),
        source_branch,
        source_revision,
        target_branch,
        target_revision,
        common_ancestor,
    )
    .await?
    .unwrap_or(common_ancestor);

    lore_debug!(
        "Resolved diff base {base_revision} for source branch {source_branch} revision {source_revision} and target branch {target_branch} revision {target_revision}"
    );

    if base_revision.is_zero() {
        lore_warn!(
            "Found no common ancestor between branch {source_branch} revision {source_revision} and branch {target_branch} revision {target_revision}"
        );
        return Err(BranchError::from(Divergent));
    }

    Ok(base_revision)
}

/// `diff3` minus the streaming wrapper. Resolves the common ancestor
/// from branch points and then uses `revision::diff3_collect` to calculate
/// the set of changes between branches with respect to the common ancestor
/// as the base revision. Auto-resolve runs over the full conflict set if
/// `auto_resolve` is true.
#[allow(clippy::too_many_arguments)]
pub async fn diff3_collect(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    auto_resolve: bool,
) -> Result<DiffResult, BranchError> {
    diff3_collect_with_graft(
        repository,
        source_branch,
        source_revision,
        target_branch,
        target_revision,
        path,
        include_same,
        auto_resolve,
        None,
    )
    .await
}

/// `diff3_collect` that may adopt whole out-of-view subtrees.
///
/// `graft_view` decides which subtrees the view excludes. `None` disables
/// adoption and gives the unmodified walk.
#[allow(clippy::too_many_arguments)]
pub async fn diff3_collect_with_graft(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    auto_resolve: bool,
    graft_view: Option<Arc<crate::filter::Filter>>,
) -> Result<DiffResult, BranchError> {
    let (summary, items) = crate::util::collect_stream::collect_stream_with_summary(|tx| {
        diff3(
            repository,
            source_branch,
            source_revision,
            target_branch,
            target_revision,
            path,
            include_same,
            auto_resolve,
            graft_view,
            tx,
        )
    })
    .await?;
    Ok(revision::diff_result_from_summary_and_items(summary, items))
}

/// Where two branches meet according to their branch stacks.
#[derive(Debug)]
struct SharedBranchPoint {
    /// Branch both sides descend from.
    branch: BranchId,
    /// Revision on the shared branch the source side descends from.
    source_point: Hash,
    /// Revision on the shared branch the target side descends from.
    target_point: Hash,
}

/// Best common ancestor of two branches, for use as the base of a 3-way diff and
/// as the floor of the search for a better one.
///
/// The branch stacks name the branch both sides descend from and the revision on
/// it each side branched at, matched on branch id alone. From those two points:
///
/// * the same revision on both sides is the answer;
/// * otherwise only the higher numbered one can reach the other by following
///   parents, so that line is followed back to the lower one;
/// * failing that the branch was rewritten and the two points sit on lines that
///   only meet further down, so both lines are followed past the lower point;
/// * failing that, within the search depth, the lower branch point is returned.
///
/// The last case is a best guess rather than a proven ancestor, which is the
/// right trade: the two points are tens of thousands of revisions apart on a busy
/// default branch, and the branches were created from that point whatever a
/// rewrite has since done to the line it sits on.
///
/// `None` only when the stacks name no branch in common. Every branch is created
/// from another, so that is an invalid branch configuration rather than a history
/// the search failed on, and the caller treats it as fatal.
async fn find_common_ancestor_from_branch_points(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    source_stack: &[BranchPoint],
    target_branch: BranchId,
    target_revision: Hash,
    target_stack: &[BranchPoint],
) -> Result<Option<Hash>, BranchError> {
    lore_debug!(
        "Find common ancestor from branch points of source branch {source_branch} revision {source_revision} and target branch {target_branch} revision {target_revision}"
    );

    let Some(shared) = find_shared_branch_point(
        source_branch,
        source_revision,
        source_stack,
        target_branch,
        target_revision,
        target_stack,
    ) else {
        return Ok(None);
    };
    if shared.source_point == shared.target_point {
        lore_debug!(
            "Found common branch {} in the branch stacks, both sides at branch point {}",
            shared.branch,
            shared.source_point
        );
        return Ok(Some(shared.source_point));
    }

    let (source_number, target_number) = join!(
        revision_number_or_zero(repository.clone(), shared.source_point),
        revision_number_or_zero(repository.clone(), shared.target_point)
    );
    lore_debug!(
        "Found common branch {} in the branch stacks, source branch point {} -> {source_number} and target branch point {} -> {target_number}",
        shared.branch,
        shared.source_point,
        shared.target_point
    );
    // Only the higher numbered point can reach the other, a revision's number
    // being one past its parents'. On equal numbers the source is taken as the
    // newer and so the target as the older, by definition rather than by
    // measurement: neither reaches the other, and the target is the side being
    // diffed into.
    let (newer_point, newer_number, older_point, older_number) = if source_number >= target_number {
        (
            shared.source_point,
            source_number,
            shared.target_point,
            target_number,
        )
    } else {
        (
            shared.target_point,
            target_number,
            shared.source_point,
            source_number,
        )
    };

    // TODO(mjansson): By keeping branch epochs and sequentially force push, we can
    //                 avoid trying to detect divergence here if branch points are known
    //                 to be from the same epoch
    let line_search = if source_number == target_number {
        // Two revisions of one number are never one another's ancestor, so the
        // points are divergent on the numbers alone and following the line would
        // spend its whole budget establishing that.
        HistoryLineSearch::Diverged
    } else {
        Box::pin(find_revision_in_history_line(
            repository.clone(),
            newer_point,
            newer_number,
            older_point,
            older_number,
        ))
        .await?
    };

    match line_search {
        HistoryLineSearch::Reached => return Ok(Some(older_point)),
        HistoryLineSearch::Diverged => {
            if let Some(shared_revision) = Box::pin(find_common_revision_in_history_lines(
                repository,
                newer_point,
                older_point,
            ))
            .await?
            {
                lore_debug!("Branch point lines meet at {shared_revision}");
                return Ok(Some(shared_revision));
            }
        }
        HistoryLineSearch::Exhausted => {
            lore_debug!("Skipping the search for where the lines meet, the budget is spent");
        }
    }

    lore_warn!(
        "Found no revision shared by branch point {newer_point} -> {newer_number} and branch point {older_point} -> {older_number} on common branch {}, using {older_point} -> {older_number} as the common ancestor",
        shared.branch
    );

    Ok(Some(older_point))
}

/// Match the two branch stacks to find the branch they share and the revision on
/// it each side descends from. `None` means the stacks share no branch, which
/// cannot happen for branches created from one another.
///
/// A branch is its own shared branch when the other side's stack names it: the
/// point on that side is then its tip, since the branch carries all of its own
/// history.
fn find_shared_branch_point(
    source_branch: BranchId,
    source_revision: Hash,
    source_stack: &[BranchPoint],
    target_branch: BranchId,
    target_revision: Hash,
    target_stack: &[BranchPoint],
) -> Option<SharedBranchPoint> {
    if let Some(source_parent) = source_stack
        .iter()
        .find(|source_parent| source_parent.branch == target_branch)
    {
        return Some(SharedBranchPoint {
            branch: target_branch,
            source_point: source_parent.revision,
            target_point: target_revision,
        });
    }

    for target_parent in target_stack.iter() {
        if target_parent.branch == source_branch {
            return Some(SharedBranchPoint {
                branch: source_branch,
                source_point: source_revision,
                target_point: target_parent.revision,
            });
        }

        if let Some(source_parent) = source_stack
            .iter()
            .find(|source_parent| source_parent.branch == target_parent.branch)
        {
            return Some(SharedBranchPoint {
                branch: target_parent.branch,
                source_point: source_parent.revision,
                target_point: target_parent.revision,
            });
        }
    }

    None
}

async fn revision_number_or_zero(repository: Arc<RepositoryContext>, revision: Hash) -> u64 {
    match State::deserialize(repository, revision).await {
        Ok(state) => state.revision_number(),
        Err(err) => {
            lore_warn!("Could not read revision {revision} to bound a history search: {err}");
            0
        }
    }
}

/// Outcome of following one history line back to a revision.
#[derive(Debug, PartialEq, Eq)]
enum HistoryLineSearch {
    /// The line reached the revision, so the two are on one line.
    Reached,
    /// The line passed below the revision's number without reaching it, so the
    /// two are on lines that split somewhere further down.
    Diverged,
    /// The walk stopped at its revision budget, which proves neither.
    Exhausted,
}

/// Follow `newer_revision`'s history line back to `older_revision` to establish
/// that the two sit on one line, which makes the older one their base.
///
/// Self parents only. A revision merged in from elsewhere is not on this line,
/// and `find_common_ancestor_from_merges` is what reaches those.
///
/// Only this direction can succeed: a revision's number is one past its parents',
/// so a walk backwards never reaches a higher numbered revision. The walk stops
/// once the line drops below `older_revision_number`, and after
/// `MAX_DIVERGENT_HISTORY_LENGTH` revisions.
///
/// The two ways of not finding it are worth telling apart.
/// [`HistoryLineSearch::Diverged`] is a result: the line went past where the
/// revision would have been. [`HistoryLineSearch::Exhausted`] is an absence of
/// one, and searching the two lines for where they meet - which is bounded the
/// same way - cannot do better from there.
async fn find_revision_in_history_line(
    repository: Arc<RepositoryContext>,
    newer_revision: Hash,
    newer_revision_number: u64,
    older_revision: Hash,
    older_revision_number: u64,
) -> Result<HistoryLineSearch, BranchError> {
    lore_debug!(
        "Follow {newer_revision} -> {newer_revision_number} back to {older_revision} -> {older_revision_number}"
    );

    if newer_revision == older_revision {
        return Ok(HistoryLineSearch::Reached);
    }

    let mut line = load_history_line(repository.clone(), newer_revision).await;
    let mut scanned = 0;

    loop {
        if line[scanned..].contains(&older_revision) {
            lore_debug!(
                "Revision {older_revision} -> {older_revision_number} is on the line from {newer_revision}"
            );
            return Ok(HistoryLineSearch::Reached);
        }
        scanned = line.len();

        if line.len() >= MAX_DIVERGENT_HISTORY_LENGTH {
            lore_warn!(
                "Reached maximum history depth of {MAX_DIVERGENT_HISTORY_LENGTH} following {newer_revision} back to {older_revision}"
            );
            return Ok(HistoryLineSearch::Exhausted);
        }

        if load_additional_history(repository.clone(), &mut line, older_revision_number).await {
            if line[scanned..].contains(&older_revision) {
                lore_debug!(
                    "Revision {older_revision} -> {older_revision_number} is on the line from {newer_revision}"
                );
                return Ok(HistoryLineSearch::Reached);
            }

            lore_debug!(
                "Line from {newer_revision} passed revision number {older_revision_number} without reaching {older_revision}"
            );
            return Ok(HistoryLineSearch::Diverged);
        }
    }
}

/// Follow both history lines back until they meet, for the case where neither
/// revision is on the other's line because the branch under them was rewritten.
///
/// Self parents only, on both sides, so the revision found is where the two lines
/// converge rather than the newest revision the two can reach through merges.
///
/// Both lines are followed to the root revisions, each capped at
/// `MAX_DIVERGENT_HISTORY_LENGTH`. `Ok(None)` means the cap was reached first, or
/// that the lines genuinely share nothing.
async fn find_common_revision_in_history_lines(
    repository: Arc<RepositoryContext>,
    self_revision: Hash,
    other_revision: Hash,
) -> Result<Option<Hash>, BranchError> {
    lore_debug!("Follow {self_revision} and {other_revision} back until the lines meet");

    // Both lines are fetched at once. Each is a round trip when the history is not
    // cached, and neither depends on the other.
    let (mut self_line, mut other_line) = join!(
        load_history_line(repository.clone(), self_revision),
        load_history_line(repository.clone(), other_revision)
    );

    // Each line keeps a lookup of itself, extended as the line is, so a round
    // tests the revisions it just loaded against everything the other side holds
    // without rebuilding anything or rewalking a pair.
    // Spelled out rather than aliased: a type alias naming a `HashSet` with a
    // hasher is an item cbindgen parses, and it cannot represent a second type
    // parameter on it.
    let mut self_lookup: std::collections::HashSet<
        Hash,
        std::hash::BuildHasherDefault<RevisionHasher>,
    > = self_line.iter().copied().collect();
    let mut other_lookup: std::collections::HashSet<
        Hash,
        std::hash::BuildHasherDefault<RevisionHasher>,
    > = other_line.iter().copied().collect();

    let mut self_ended = false;
    let mut other_ended = false;
    let mut self_loaded = 0;
    let mut other_loaded = 0;

    loop {
        // Scanning the line rather than the lookup is what makes this the newest
        // shared revision: the line is in history order, a set is in neither.
        if let Some(shared) = self_line[self_loaded..]
            .iter()
            .find(|revision| other_lookup.contains(*revision))
            .copied()
        {
            return Ok(Some(shared));
        }
        if let Some(shared) = other_line[other_loaded..]
            .iter()
            .find(|revision| self_lookup.contains(*revision))
            .copied()
        {
            return Ok(Some(shared));
        }
        self_loaded = self_line.len();
        other_loaded = other_line.len();

        if self_ended && other_ended {
            return Ok(None);
        }

        let extend_self = self_line.len() < MAX_DIVERGENT_HISTORY_LENGTH && !self_ended;
        let extend_other = other_line.len() < MAX_DIVERGENT_HISTORY_LENGTH && !other_ended;
        if !extend_self && !extend_other {
            lore_warn!(
                "Reached maximum history depth of {MAX_DIVERGENT_HISTORY_LENGTH} following {self_revision} and {other_revision} back to a shared revision"
            );
            return Ok(None);
        }

        // Two round trips that need not wait on each other.
        join!(
            async {
                if extend_self {
                    self_ended =
                        load_additional_history(repository.clone(), &mut self_line, 0).await;
                    self_lookup.extend(self_line[self_loaded..].iter().copied());
                }
            },
            async {
                if extend_other {
                    other_ended =
                        load_additional_history(repository.clone(), &mut other_line, 0).await;
                    other_lookup.extend(other_line[other_loaded..].iter().copied());
                }
            }
        );
    }
}

/// A revision hash is already uniformly distributed, so its leading bytes are as
/// good a bucket index as anything derived from all 32 - and free. Hashing the
/// hash again is the cost this avoids.
#[derive(Default)]
struct RevisionHasher(u64);

impl std::hash::Hasher for RevisionHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut leading = [0u8; 8];
        let taken = bytes.len().min(leading.len());
        leading[..taken].copy_from_slice(&bytes[..taken]);
        self.0 = u64::from_ne_bytes(leading);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Load a line of revisions from `revision` backwards through self parents. Falls
/// back to a local walk when the batch load comes back empty, so a repository
/// with no reachable remote still contributes the line it has cached.
async fn load_history_line(repository: Arc<RepositoryContext>, revision: Hash) -> Vec<Hash> {
    let mut history = find::batch_load_history(repository.clone(), revision).await;
    if !history.is_empty() {
        // The caller extends this a batch at a time up to the search depth, so
        // take the growth in one allocation rather than a copy per doubling.
        history.reserve(MAX_DIVERGENT_HISTORY_LENGTH.saturating_sub(history.len()));
        return history;
    }

    lore_debug!("Found no revision from {revision}, walking locally");

    let mut history = Vec::new();
    let Ok(mut state_iter) = state::State::deserialize(repository.clone(), revision).await else {
        return history;
    };
    history.push(revision);

    while history.len() < find::BATCH_COUNT && !state_iter.parent_self().is_zero() {
        let revision_next = state_iter.parent_self();
        let Ok(state_next) = state::State::deserialize(repository.clone(), revision_next).await
        else {
            break;
        };
        history.push(revision_next);
        state_iter = state_next;
    }

    history
}

/// Whether a line has been followed to or past `floor_revision_number`, leaving
/// nothing below it worth loading. A line that cannot be read any further counts
/// as reached for the same reason.
async fn history_reached_floor(
    repository: Arc<RepositoryContext>,
    history: &[Hash],
    floor_revision_number: u64,
) -> bool {
    let Some(oldest) = history.last() else {
        return true;
    };

    let Ok(state) = State::deserialize(repository, *oldest).await else {
        return true;
    };

    state.revision_number() <= floor_revision_number
}

/// Extend a line with the next batch of older revisions. Returns whether the line
/// has nothing further worth loading, either because it reached the floor or
/// because it ran out.
async fn load_additional_history(
    repository: Arc<RepositoryContext>,
    history: &mut Vec<Hash>,
    floor_revision_number: u64,
) -> bool {
    let Some(&last) = history.last() else {
        return true;
    };

    let additional = find::batch_load_history(repository.clone(), last).await;

    // `batch_load_history` yields its start revision first, and that revision is
    // already the tail of `history`. Appending it again both duplicates work for
    // the caller's comparison and, once the line is exhausted, keeps reporting
    // progress that does not exist.
    let fresh = match additional.split_first() {
        Some((first, rest)) if *first == last => rest,
        _ => additional.as_slice(),
    };

    if fresh.is_empty() {
        return true;
    }

    // Appended before the floor is tested, since this batch still has to be
    // compared - it can hold the revision being searched for.
    history.extend_from_slice(fresh);

    history_reached_floor(repository, history, floor_revision_number).await
}

struct WalkVisit {
    source: bool,
    target: bool,
}

/// State shared by every walker of one `find_common_ancestor_from_merges` search.
struct AncestorWalk {
    /// Which of the two walks has reached each revision. A revision reached by
    /// both is a common ancestor.
    visited: DashMap<Hash, WalkVisit>,
    /// Highest-numbered revision reached by both walks. Never seeded with the
    /// caller's floor, so a search that finds nothing stays distinguishable from
    /// one that finds the floor.
    common: RwLock<Option<(u64, Hash)>>,
    /// Revision the walks stop at, from the caller's base revision. Zero
    /// searches the whole history.
    floor_revision: Hash,
    /// Revision number of [`Self::floor_revision`], which the walks may not go
    /// below.
    floor_revision_number: u64,
    /// First read failure that cut a walk short, kept so the search can report
    /// an incomplete history rather than an absent common ancestor.
    failure: RwLock<Option<StateError>>,
}

impl AncestorWalk {
    fn new(floor_revision: Hash, floor_revision_number: u64) -> Self {
        Self {
            visited: DashMap::new(),
            common: RwLock::new(None),
            floor_revision,
            floor_revision_number,
            failure: RwLock::new(None),
        }
    }

    async fn prune_at_or_below(&self) -> u64 {
        match *self.common.read().await {
            Some((found_revision_number, _found_revision)) => {
                std::cmp::max(found_revision_number, self.floor_revision_number)
            }
            None => self.floor_revision_number,
        }
    }
}

async fn find_ancestor_walker(
    repository: Arc<RepositoryContext>,
    revision_start: Hash,
    walk: Arc<AncestorWalk>,
    is_target: bool,
) {
    let mut tasks = JoinSet::new();

    let mut revision = revision_start;
    while revision != Hash::default() {
        let state = match state::State::deserialize(repository.clone(), revision).await {
            Ok(state) => state,
            Err(err) => {
                lore_warn!("Could not deserialize state for {revision} - aborting walk: {err}");
                let mut failure = walk.failure.write().await;
                if failure.is_none() {
                    *failure = Some(err);
                }
                break;
            }
        };

        // Guard scope is this statement and no arm awaits, so the concurrent
        // sibling walkers sharing `visited` cannot build a wait cycle.
        #[allow(clippy::disallowed_methods)]
        let both_visited = match walk.visited.entry(revision) {
            Entry::Occupied(mut visited) => {
                let visited = visited.get_mut();
                if is_target {
                    // If encountering a circular dependency, iteration can stop.
                    if visited.target {
                        break;
                    }
                    visited.target = true;
                } else {
                    // If encountering a circular dependency, iteration can stop.
                    if visited.source {
                        break;
                    }
                    visited.source = true;
                }
                visited.source && visited.target
            }
            Entry::Vacant(entry) => {
                entry.insert(WalkVisit {
                    source: !is_target,
                    target: is_target,
                });
                false
            }
        };

        let state_revision = state.revision();
        let state_revision_number = state.revision_number();

        if both_visited {
            let mut common = walk.common.write().await;

            if let Some((found_revision_number, _found_revision)) = *common {
                if state_revision_number > found_revision_number {
                    *common = Some((state_revision_number, state_revision));
                }
            } else {
                *common = Some((state_revision_number, state_revision));
            }

            break;
        }

        if state_revision_number <= walk.prune_at_or_below().await {
            break;
        }

        if revision == walk.floor_revision {
            break;
        }

        let parent_other = state.parent_other();
        if parent_other != Hash::default() {
            lore_spawn!(tasks, {
                let repository = repository.clone();
                let walk = walk.clone();
                async move {
                    find_ancestor_walker_recurse(repository, parent_other, walk, is_target).await;
                }
            });
        }

        revision = state.parent_self();
    }

    while let Some(_result) = tasks.join_next().await {}
}

fn find_ancestor_walker_recurse(
    repository: Arc<RepositoryContext>,
    revision_start: Hash,
    walk: Arc<AncestorWalk>,
    is_target: bool,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(find_ancestor_walker(
        repository,
        revision_start,
        walk,
        is_target,
    ))
}

/// Walk both branches' histories backwards for the newest revision reachable
/// from both, which is the best available 3-way base.
///
/// Both parents, so a revision merged in from elsewhere counts as reachable. That
/// is what lets this improve on a branch point: a merge that already carried one
/// branch into the other leaves a revision newer than where they parted.
///
/// `base_revision` is a floor, not an answer: the walks stop there and prune any
/// revision at or below its revision number. Pass `Hash::default()` to search
/// with no floor.
///
/// Returns `Ok(None)` when the walks completed without finding anything above
/// the floor, so the caller keeps its own base. Returns `Err` when a revision
/// state could not be read and no common ancestor was found, so a transient read
/// failure is never reported as an absent common ancestor.
async fn find_common_ancestor_from_merges(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
    base_revision: Hash,
) -> Result<Option<Hash>, BranchError> {
    let mut tasks = JoinSet::new();

    let floor_revision_number = match State::deserialize(repository.clone(), base_revision).await {
        Ok(state) => state.revision_number(),
        Err(err) => {
            // Leaves the search unbounded, which still answers correctly but walks
            // both histories to their root revisions.
            lore_warn!(
                "Could not read base revision {base_revision} to bound the common ancestor search: {err}"
            );
            0
        }
    };

    lore_debug!(
        "Find common ancestor from merges of source branch {source_branch} revision {source_revision} and target branch {target_branch} revision {target_revision}, above base revision {base_revision} -> {floor_revision_number}"
    );

    let walk = Arc::new(AncestorWalk::new(base_revision, floor_revision_number));

    lore_spawn!(tasks, {
        let repository = repository.clone();
        let walk = walk.clone();
        let is_target = false;
        async move {
            lore_debug!(
                "Walking backwards on source branch {source_branch} from {source_revision}"
            );

            find_ancestor_walker(repository, source_revision, walk, is_target).await;
        }
    });

    lore_spawn!(tasks, {
        let repository = repository.clone();
        let walk = walk.clone();
        let is_target = true;
        async move {
            lore_debug!(
                "Walking backwards on target branch {target_branch} from {target_revision}"
            );

            find_ancestor_walker(repository, target_revision, walk, is_target).await;
        }
    });

    while let Some(_result) = tasks.join_next().await {}

    let found = *walk.common.read().await;
    let failure = walk.failure.write().await.take();

    match (found, failure) {
        (Some((revision_number, revision)), failure) => {
            if let Some(err) = failure {
                lore_warn!(
                    "Revision {revision} -> {revision_number} found as common ancestor, but part of the history could not be read, so a newer common ancestor may exist: {err}"
                );
            } else {
                lore_debug!("Revision {revision} -> {revision_number} found as common ancestor");
            }

            Ok(Some(revision))
        }
        (None, Some(err)) => Err(err).forward::<BranchError>(
            "Failed to read revision history while searching for a common ancestor",
        ),
        (None, None) => {
            lore_debug!(
                "Walk found no common ancestor newer than revision {base_revision} -> {floor_revision_number}, keeping it as base"
            );

            Ok(None)
        }
    }
}

pub fn dispatch_diff_events(diff: &DiffResult) {
    event::LoreEvent::BranchDiffBegin(LoreBranchDiffBeginEventData::default()).send();

    event::LoreEvent::BranchDiffChangeBegin(LoreBranchDiffChangeBeginEventData {
        changes_count: diff.changes.len(),
    })
    .send();

    for change in diff.changes.iter() {
        event::LoreEvent::BranchDiffChange(LoreBranchDiffChangeEventData {
            change: LoreBranchDiffNodeData::new(change),
        })
        .send();
    }

    event::LoreEvent::BranchDiffChangeEnd(LoreBranchDiffChangeEndEventData::default()).send();

    event::LoreEvent::BranchDiffConflictBegin(LoreBranchDiffConflictBeginEventData {
        conflicts_count: diff.conflicts.len(),
    })
    .send();

    for conflict in diff.conflicts.iter() {
        event::LoreEvent::BranchDiffConflict(LoreBranchDiffConflictEventData {
            source_change: LoreBranchDiffNodeData::new(&conflict.0),
            target_change: LoreBranchDiffNodeData::new(&conflict.1),
        })
        .send();
    }

    event::LoreEvent::BranchDiffConflictEnd(LoreBranchDiffConflictEndEventData::default()).send();

    event::LoreEvent::BranchDiffEnd(LoreBranchDiffEndEventData::default()).send();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::path::RelativePathBuf;

    fn branch_id(byte: u8) -> BranchId {
        BranchId::from([byte; 16])
    }

    fn revision(byte: u8) -> Hash {
        Hash::from([byte; 32])
    }

    fn branch_point(branch: u8, revision_byte: u8) -> BranchPoint {
        BranchPoint {
            branch: branch_id(branch),
            revision: revision(revision_byte),
        }
    }

    /// A branch merging the branch it was created from: the shared branch is the
    /// target itself, so the target side contributes its tip and the source side
    /// the revision it branched at.
    #[test]
    fn shared_branch_point_when_target_is_a_parent_of_source() {
        let shared = find_shared_branch_point(
            branch_id(1),
            revision(10),
            &[branch_point(2, 20), branch_point(3, 30)],
            branch_id(2),
            revision(21),
            &[branch_point(3, 30)],
        )
        .expect("The target branch is named by the source stack");

        assert_eq!(
            shared.branch,
            branch_id(2),
            "The target branch is the shared one"
        );
        assert_eq!(shared.source_point, revision(20));
        assert_eq!(shared.target_point, revision(21));
    }

    /// The same relation seen from the other side.
    #[test]
    fn shared_branch_point_when_source_is_a_parent_of_target() {
        let shared = find_shared_branch_point(
            branch_id(2),
            revision(21),
            &[branch_point(3, 30)],
            branch_id(1),
            revision(10),
            &[branch_point(2, 20), branch_point(3, 30)],
        )
        .expect("The source branch is named by the target stack");

        assert_eq!(
            shared.branch,
            branch_id(2),
            "The source branch is the shared one"
        );
        assert_eq!(shared.source_point, revision(21));
        assert_eq!(shared.target_point, revision(20));
    }

    /// Siblings created from the default branch, which is where both stacks end.
    #[test]
    fn sibling_branches_of_the_root_branch_meet_at_their_branch_points() {
        let shared = find_shared_branch_point(
            branch_id(1),
            revision(10),
            &[branch_point(9, 20)],
            branch_id(2),
            revision(11),
            &[branch_point(9, 21)],
        )
        .expect("Both stacks name the same branch");

        assert_eq!(
            shared.branch,
            branch_id(9),
            "A third branch both descend from"
        );
        assert_eq!(shared.source_point, revision(20));
        assert_eq!(shared.target_point, revision(21));
    }

    /// Branch points are matched on branch id, so stacks recording different
    /// revisions below the shared branch make no difference to which branch is
    /// shared or which points are returned.
    #[test]
    fn stacks_are_matched_on_branch_id_alone() {
        let shared = find_shared_branch_point(
            branch_id(1),
            revision(10),
            &[branch_point(5, 20), branch_point(9, 30)],
            branch_id(2),
            revision(11),
            &[branch_point(5, 21), branch_point(9, 31)],
        )
        .expect("Both stacks name branch 5");

        assert_eq!(shared.source_point, revision(20));
        assert_eq!(shared.target_point, revision(21));
    }

    /// The nearest shared branch wins, not the deepest.
    #[test]
    fn nearest_shared_branch_is_matched_first() {
        let shared = find_shared_branch_point(
            branch_id(1),
            revision(10),
            &[branch_point(5, 20), branch_point(9, 30)],
            branch_id(2),
            revision(11),
            &[branch_point(5, 21), branch_point(9, 30)],
        )
        .expect("Both stacks name branch 5 and branch 9");

        assert_eq!(
            shared.source_point,
            revision(20),
            "Branch 5 is nearer than branch 9 and has to be the shared branch"
        );
    }

    #[test]
    fn stacks_sharing_no_branch_are_unresolvable() {
        assert!(
            find_shared_branch_point(
                branch_id(1),
                revision(10),
                &[branch_point(5, 20)],
                branch_id(2),
                revision(11),
                &[branch_point(6, 21)],
            )
            .is_none(),
            "Stacks naming unrelated branches cannot be resolved"
        );
    }

    #[test]
    fn empty_stacks_are_unresolvable() {
        assert!(
            find_shared_branch_point(
                branch_id(1),
                revision(10),
                &[],
                branch_id(2),
                revision(11),
                &[],
            )
            .is_none(),
            "Without stacks there is nothing to match"
        );
    }

    /// Run a body with an execution context installed, which the revision store
    /// reads through `execution_context()`.
    async fn with_execution<F: Future>(body: F) -> F::Output {
        let execution = Arc::new(crate::interface::ExecutionContext::new_client(
            crate::interface::LoreGlobalArgs::default(),
            crate::relay::EventDispatcher::no_dispatch(),
        ));
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, body)
            .await
    }

    /// A repository backed by in-memory stores and no remote, so every read has to
    /// come from the revisions the test wrote.
    async fn null_repository() -> Arc<RepositoryContext> {
        let immutable_store = lore_storage::local::immutable_store::create(
            None::<&str>,
            lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
            false,
            lore_storage::ImmutableStoreSettings::default(),
        )
        .await
        .expect("in-memory immutable store");
        let mutable_store = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            immutable_store.clone(),
        )
        .await
        .expect("in-memory mutable store");

        Arc::new(RepositoryContext::new_null_context(
            immutable_store,
            mutable_store,
        ))
    }

    /// Write a revision carrying nothing but its parent and number, which is all
    /// the history search reads.
    async fn write_revision(
        repository: &Arc<RepositoryContext>,
        parent: Hash,
        revision_number: u64,
    ) -> Hash {
        let token = repository
            .try_write_token()
            .expect("a null context carries a write token");
        // Behind an `Arc`, as every other holder of a `State` has it: a bare one
        // is held across the serialize await, which puts the whole of it in this
        // future rather than a pointer to it.
        let state = Arc::new(State::new());
        state.set_parent_self(parent);
        state.set_revision_number(revision_number);
        state
            .serialize(repository.clone(), token)
            .await
            .expect("serializing the revision state")
    }

    /// Write a line of `count` revisions numbered from `first_revision_number`,
    /// returning them oldest first.
    async fn write_line(
        repository: &Arc<RepositoryContext>,
        parent: Hash,
        first_revision_number: u64,
        count: u64,
    ) -> Vec<Hash> {
        let mut line = Vec::new();
        let mut parent = parent;
        for offset in 0..count {
            parent = write_revision(repository, parent, first_revision_number + offset).await;
            line.push(parent);
        }
        line
    }

    /// The branch point of a branch that has not moved since it was created is
    /// still on the line of the branch it was created from.
    #[tokio::test]
    async fn history_line_reaches_the_older_revision() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let line = write_line(&repository, Hash::default(), 1, 3).await;

            let found = find_revision_in_history_line(repository, line[2], 3, line[0], 1)
                .await
                .expect("the search must not fail on readable lines");

            assert_eq!(found, HistoryLineSearch::Reached);
        }))
        .await;
    }

    /// Two branch points at the same revision need no search at all.
    #[tokio::test]
    async fn history_line_reaches_the_same_revision_on_both_sides() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let line = write_line(&repository, Hash::default(), 1, 2).await;

            let found = find_revision_in_history_line(repository, line[1], 2, line[1], 2)
                .await
                .expect("the search must not fail");

            assert_eq!(found, HistoryLineSearch::Reached);
        }))
        .await;
    }

    /// The rewritten-branch shape: two revisions on sibling lines under a common
    /// parent. The line passes the sibling's number without reaching it, which is
    /// what tells the caller to go looking for where the lines meet.
    #[tokio::test]
    async fn history_line_diverges_from_a_sibling_line() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 2).await;
            let newer = write_revision(&repository, shared[1], 13).await;
            let older = write_revision(&repository, shared[1], 3).await;

            let found = find_revision_in_history_line(repository, newer, 13, older, 3)
                .await
                .expect("passing the older revision's number is not a failure");

            assert_eq!(
                found,
                HistoryLineSearch::Diverged,
                "The sibling is not on the line, and claiming it is would put an unproven base in the diff"
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn history_line_diverges_from_a_line_sharing_no_revision() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let newer = write_line(&repository, Hash::default(), 11, 3).await;
            let older = write_line(&repository, Hash::default(), 1, 3).await;

            let found = find_revision_in_history_line(
                repository,
                *newer.last().unwrap(),
                13,
                *older.last().unwrap(),
                3,
            )
            .await
            .expect("running out of line is not a failure");

            assert_eq!(
                found,
                HistoryLineSearch::Diverged,
                "Reporting a base here would put a revision in the diff that is on neither line"
            );
        }))
        .await;
    }

    /// Two lines that split under a shared parent meet again at that parent.
    #[tokio::test]
    async fn history_lines_meet_where_they_split() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 2).await;
            let one = write_revision(&repository, shared[1], 13).await;
            let other = write_revision(&repository, shared[1], 3).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one, other,
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(
                met,
                Some(shared[1]),
                "The revision the two lines split at is the newest they share"
            );
        }))
        .await;
    }

    /// The newest shared revision wins, not the first one reached on either line.
    #[tokio::test]
    async fn history_lines_meet_at_the_newest_shared_revision() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 3).await;
            let one = write_revision(&repository, shared[2], 13).await;
            let other = write_revision(&repository, shared[2], 4).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one, other,
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(met, Some(shared[2]));
        }))
        .await;
    }

    #[tokio::test]
    async fn history_lines_that_share_no_revision_meet_nowhere() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let one = write_line(&repository, Hash::default(), 1, 3).await;
            let other = write_line(&repository, Hash::default(), 11, 3).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository,
                *one.last().unwrap(),
                *other.last().unwrap(),
            ))
            .await
            .expect("running out of line is not a failure");

            assert_eq!(met, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn history_lines_that_cannot_be_read_meet_nowhere() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository,
                revision(200),
                revision(201),
            ))
            .await
            .expect("an unreadable line is not a failure of the search");

            assert_eq!(met, None);
        }))
        .await;
    }

    /// A branch that has not moved since it was created: its branch point is still
    /// on the line of the branch it was created from, so that point is the answer.
    #[tokio::test]
    async fn common_ancestor_is_the_branch_point_still_on_the_line() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let main_line = write_line(&repository, Hash::default(), 1, 4).await;

            let found = find_common_ancestor_from_branch_points(
                repository,
                branch_id(1),
                revision(10),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: main_line[1],
                }],
                branch_id(9),
                main_line[3],
                &[],
            )
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(
                found,
                Some(main_line[1]),
                "The branch point is reached by following the shared branch back"
            );
        }))
        .await;
    }

    /// A rewritten shared branch: the two points sit on lines that split below
    /// them, and the answer is where those lines meet.
    #[tokio::test]
    async fn common_ancestor_is_where_rewritten_lines_meet() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 2).await;
            let source_point = write_revision(&repository, shared[1], 13).await;
            let target_point = write_revision(&repository, shared[1], 3).await;

            let found = find_common_ancestor_from_branch_points(
                repository,
                branch_id(1),
                revision(10),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: source_point,
                }],
                branch_id(2),
                revision(11),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: target_point,
                }],
            )
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(found, Some(shared[1]));
        }))
        .await;
    }

    /// Branch points of equal revision number cannot reach one another, whatever
    /// their hashes. The answer still comes from following both lines to where
    /// they meet, rather than from taking one of the points.
    #[tokio::test]
    async fn common_ancestor_of_equal_numbered_points_is_where_lines_meet() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            // Same number, different parents, so the two are distinct revisions
            // that provably cannot reach one another.
            let shared = write_line(&repository, Hash::default(), 1, 2).await;
            let source_point = write_revision(&repository, shared[1], 3).await;
            let target_point = write_revision(&repository, shared[0], 3).await;

            let found = find_common_ancestor_from_branch_points(
                repository,
                branch_id(1),
                revision(10),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: source_point,
                }],
                branch_id(2),
                revision(11),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: target_point,
                }],
            )
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(
                found,
                Some(shared[0]),
                "The lines meet at the revision they both descend from"
            );
        }))
        .await;
    }

    /// Nothing found either way still answers, with the older of the two branch
    /// points. It is a guess, and it beats refusing a diff the caller cannot
    /// repair.
    #[tokio::test]
    async fn common_ancestor_falls_back_to_the_older_branch_point() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let source_line = write_line(&repository, Hash::default(), 11, 2).await;
            let target_line = write_line(&repository, Hash::default(), 1, 2).await;

            let found = find_common_ancestor_from_branch_points(
                repository,
                branch_id(1),
                revision(10),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: source_line[1],
                }],
                branch_id(2),
                revision(11),
                &[BranchPoint {
                    branch: branch_id(9),
                    revision: target_line[1],
                }],
            )
            .await
            .expect("falling back is not a failure");

            assert_eq!(
                found,
                Some(target_line[1]),
                "The lower numbered branch point is the guess, and it is never zero"
            );
        }))
        .await;
    }

    /// Stacks naming no branch in common are the one case with no answer, which
    /// the caller reports as an invalid branch configuration.
    #[tokio::test]
    async fn common_ancestor_is_absent_only_for_unshared_stacks() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;

            let found = find_common_ancestor_from_branch_points(
                repository,
                branch_id(1),
                revision(10),
                &[BranchPoint {
                    branch: branch_id(5),
                    revision: revision(20),
                }],
                branch_id(2),
                revision(11),
                &[BranchPoint {
                    branch: branch_id(6),
                    revision: revision(21),
                }],
            )
            .await
            .expect("unshared stacks are not a failure of the search");

            assert_eq!(found, None);
        }))
        .await;
    }

    /// An unreadable line cannot be followed, and the search says so rather than
    /// answering with the revision it was asked to look for.
    #[tokio::test]
    async fn history_line_that_cannot_be_read_diverges() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;

            let found =
                find_revision_in_history_line(repository, revision(200), 2, revision(201), 1)
                    .await
                    .expect("an unreadable line is not a failure of the search");

            assert_eq!(found, HistoryLineSearch::Diverged);
        }))
        .await;
    }

    #[tokio::test]
    async fn history_line_of_an_unknown_revision_is_empty() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;

            assert!(
                load_history_line(repository, revision(200))
                    .await
                    .is_empty(),
                "A revision that is not stored yields no line to search"
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn reaching_the_floor_needs_a_readable_line_below_it() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let line = write_line(&repository, Hash::default(), 1, 3).await;

            assert!(
                history_reached_floor(repository.clone(), &[], 0).await,
                "An empty line has nothing left to load"
            );
            assert!(
                history_reached_floor(repository.clone(), &[revision(200)], 0).await,
                "A line that cannot be read any further has nothing left to load"
            );
            assert!(
                history_reached_floor(repository.clone(), &line[..1], 1).await,
                "Revision number 1 is at a floor of 1"
            );
            assert!(
                !history_reached_floor(repository, &line[2..], 1).await,
                "Revision number 3 is above a floor of 1, so the line can still be followed"
            );
        }))
        .await;
    }

    /// Extending a line that has reached its root revision adds nothing. Reporting
    /// progress instead would keep the caller re-comparing the same line until its
    /// depth limit stopped it.
    #[tokio::test]
    async fn extending_an_exhausted_line_reports_it_exhausted() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let line = write_line(&repository, Hash::default(), 1, 2).await;
            let mut history = vec![line[1], line[0]];

            assert!(
                load_additional_history(repository.clone(), &mut history, 0).await,
                "A line followed to its root revision has nothing further to load"
            );
            assert_eq!(
                history,
                vec![line[1], line[0]],
                "An exhausted line must not grow"
            );

            let mut empty = Vec::new();
            assert!(
                load_additional_history(repository, &mut empty, 0).await,
                "There is nothing to extend an empty line from"
            );
            assert!(empty.is_empty());
        }))
        .await;
    }

    /// A change between two nodes of one kind, carrying the path a move or copy
    /// came from.
    fn node_change(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        action: FileAction,
        flags: NodeFlags,
        path: &str,
        from_path: Option<&str>,
    ) -> NodeChange {
        let side = |node| change::NodeChangeState {
            repository: repository.clone(),
            state: state.clone(),
            node,
            flags,
            address: Address::default(),
        };
        NodeChange {
            action,
            flags: change::Flags::None,
            from: side(1),
            to: side(2),
            path: RelativePathBuf::new().push_and_freeze(path),
            from_path: from_path.map(|path| RelativePathBuf::new().push_and_freeze(path)),
        }
    }

    /// Without the source path a receiver reads a move as an add at the new path
    /// and cannot tell where the content came from.
    #[tokio::test]
    async fn diff_change_carries_the_move_source_path() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let state = Arc::new(State::new());
            let change = node_change(
                &repository,
                &state,
                FileAction::Move,
                NodeFlags::File,
                "new.txt",
                Some("old.txt"),
            );

            let data = LoreBranchDiffNodeData::new(&change);

            assert_eq!(data.path.as_str(), "new.txt");
            assert_eq!(data.from_path.as_str(), "old.txt");
        }))
        .await;
    }

    /// A change that moved nothing maps to the empty string the C API documents,
    /// not to a dangling pointer a receiver would read past.
    #[tokio::test]
    async fn diff_change_without_a_move_reports_no_source_path() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let state = Arc::new(State::new());
            let change = node_change(
                &repository,
                &state,
                FileAction::Add,
                NodeFlags::File,
                "new.txt",
                None,
            );

            let data = LoreBranchDiffNodeData::new(&change);

            assert!(data.from_path.is_empty());
            assert_eq!(data.from_path.as_str(), "");
        }))
        .await;
    }

    /// Both paths of a moved directory get the trailing separator that tells a
    /// directory from a file, so the two can be compared as they are reported.
    #[tokio::test]
    async fn diff_change_marks_a_moved_directory_on_both_paths() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let state = Arc::new(State::new());
            let change = node_change(
                &repository,
                &state,
                FileAction::Move,
                NodeFlags::NoFlags,
                "new",
                Some("old"),
            );

            let data = LoreBranchDiffNodeData::new(&change);

            assert_eq!(data.path.as_str(), "new/");
            assert_eq!(data.from_path.as_str(), "old/");
        }))
        .await;
    }
}
