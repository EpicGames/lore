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
use crate::link::LinkFlags;
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
}

impl LoreBranchDiffNodeData {
    fn new(node_change: &NodeChange) -> Self {
        let is_directory_or_module = if node_change.action == FileAction::Delete {
            !node_change.from.flags.contains(NodeFlags::File)
        } else {
            !node_change.to.flags.contains(NodeFlags::File)
        };
        Self {
            action: LoreFileAction::from(node_change.action),
            path: if is_directory_or_module {
                format!("{}/", node_change.path.as_str()).into()
            } else {
                node_change.path.as_str().into()
            },
            automerged: node_change.flags.is_conflict_automerged().into(),
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
            Self::Disconnected(_) => LoreError::Connection,
            Self::SlowDown(_) => LoreError::SlowDown,
            Self::Oversized(_) => LoreError::Oversized,
            Self::FileNotFound(_) => LoreError::FileNotFound,
            Self::NotFound(_)
            | Self::BranchNotFound(_)
            | Self::RevisionNotFound(_)
            | Self::LayerNotFound(_)
            | Self::LinkNotFound(_)
            | Self::NodeNotFound(_) => LoreError::NotFound,
            Self::AddressNotFound(_) => LoreError::AddressNotFound,
            Self::PayloadNotFound(_) => LoreError::PayloadNotFound,
            Self::InvalidPath(_) | Self::InvalidArguments(_) | Self::Divergent(_) => {
                LoreError::InvalidArguments
            }
            Self::BranchAlreadyExists(_) => LoreError::AlreadyExists,
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

pub async fn store_latest(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    status: BranchLatestStatus,
) -> Result<(), BranchError> {
    mutable_store(repository.clone(), LATEST, branch, latest).await?;

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

    let (address, _) = entry
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
    let remote = repository.remote().await.map_err(|_err| {
        BranchError::from(BranchNotFound {
            branch: branch_input.to_string(),
        })
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
        let remote = repository.remote().await.map_err(|_err| {
            BranchError::from(BranchNotFound {
                branch: branch_input.to_string(),
            })
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
    metadata
        .walk(|key, value, _value_type| {
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
        })
        .forward::<BranchError>("Failed to walk branch metadata")?;

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

    let mut link_tasks = JoinSet::new();

    for link_reference in link_list.iter() {
        if link_reference.flags & LinkFlags::DisableAutoFollow != 0 {
            lore_debug!(
                "Auto follow disabled for link {}",
                link_reference.repository
            );
            continue;
        }

        lore_spawn!(link_tasks, {
            let link_id = link_reference.repository;
            let link = Arc::new(repository.to_link_context(link_id).await);
            let link_remote = link.remote().await.forward_with::<BranchError, _>(|| {
                format!("Failed to connect to link repository {link_id}")
            })?;

            let repository = repository.clone();
            let state = state.clone();
            let branch_id = branch;
            let branch_name = name.clone();
            let branch_category = category.clone();
            let link_reference = *link_reference;

            async move {
                let resolved_parent_branch = link_reference.resolve_branch(current_branch);

                link::create_branch(
                    link.clone(),
                    link_remote,
                    branch_id,
                    branch_name,
                    branch_category,
                    resolved_parent_branch,
                    link_reference.signature,
                )
                .await
                .forward_with::<BranchError, _>(|| {
                    format!("Failed to create branch for link repository {link_id}")
                })?;

                // When the link uses the implicit branch convention (zero),
                // skip update_link_pin_by_node — the branch is already
                // implicitly correct and the signature is unchanged (the new
                // linked branch points to the same revision). This avoids
                // dirtying the state and producing a bookkeeping revision.
                if !link_reference.branch.is_zero() {
                    link::update_link_pin_by_node(
                        &state,
                        repository.clone(),
                        link_reference.repository,
                        branch_id,
                        link_reference.signature,
                        link_reference.local_node,
                    )
                    .await
                    .forward_with::<BranchError, _>(|| {
                        format!(
                            "Failed to update link reference for link repository {}",
                            link_reference.repository
                        )
                    })?;
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

    event::LoreEvent::BranchArchive(LoreBranchArchiveEventData {
        name: branch_name.into(),
    })
    .send();

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

    remote
        .branch_delete(branch)
        .await
        .forward::<BranchError>("Failed to delete branch on remote")?;

    Ok(())
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
        let mut proto_revision = Self {
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
        revision
            .metadata
            .walk(|key, value, value_type| {
                let key = std::str::from_utf8(key).unwrap_or("<binary>");
                match key {
                    metadata::MESSAGE => {
                        proto_revision.commit_message =
                            std::str::from_utf8(value).unwrap_or("<binary>").to_string();
                    }
                    metadata::TIMESTAMP => {
                        if value.len() == std::mem::size_of::<u64>() {
                            proto_revision.timestamp =
                                u64::from_le_bytes(value.try_into().unwrap());
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
            })
            .unwrap_or_default();

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
        let _ = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&theirs_path)
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
        let _ = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&base_path)
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
        let _ = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&mine_path)
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
/// Returns `Hash::default()` (zero) when no common ancestor exists; the
/// caller treats that as "disjoint histories" and surfaces an appropriate
/// error. May raise `BranchError::Divergent` or
/// `BranchError::MaxHistorySearchDepth` from the divergence search.
pub async fn resolve_diff3_base(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
) -> Result<Hash, BranchError> {
    // Find base revision from branch stack parents and branch points
    let mut base_revision = Hash::default();
    if source_branch != target_branch {
        let mut branch_source_point = Hash::default();
        let mut branch_target_point = Hash::default();
        let mut base_branch_point = Hash::default();

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

        for source_parent in source_stack.iter() {
            if target_branch == source_parent.branch {
                branch_source_point = source_parent.revision;
                branch_target_point = target_revision;
                base_branch_point = target_stack
                    .first()
                    .map(|parent| parent.revision)
                    .unwrap_or_default();
                break;
            }
        }
        if branch_source_point.is_zero() {
            for (target_index, target_parent) in target_stack.iter().enumerate() {
                if target_parent.branch == source_branch {
                    branch_target_point = target_parent.revision;
                    branch_source_point = source_revision;
                    base_branch_point = source_stack
                        .first()
                        .map(|parent| parent.revision)
                        .unwrap_or_default();
                    break;
                }
                for (source_index, source_parent) in source_stack.iter().enumerate() {
                    if target_parent.branch == source_parent.branch {
                        // Found common ancestor branch
                        branch_source_point = source_parent.revision;
                        branch_target_point = target_parent.revision;

                        // Pick the lowest numbered branch point revision as the base point
                        let next_target_index = target_index + 1;
                        let next_source_index = source_index + 1;
                        if next_target_index < target_stack.len()
                            && next_source_index < source_stack.len()
                        {
                            let source_revision = source_stack[next_source_index].revision;
                            let target_revision = target_stack[next_target_index].revision;

                            if source_revision != target_revision
                                && let Ok(source_state) =
                                    State::deserialize(repository.clone(), source_revision).await
                                && let Ok(target_state) =
                                    State::deserialize(repository.clone(), target_revision).await
                            {
                                if source_state.revision_number() < target_state.revision_number() {
                                    base_branch_point = source_revision;
                                } else {
                                    base_branch_point = target_revision;
                                }
                            } else {
                                base_branch_point = source_revision;
                            }
                        }
                        break;
                    }
                }
                if !branch_source_point.is_zero() {
                    break;
                }
            }
        }

        if !branch_source_point.is_zero() {
            if branch_source_point != branch_target_point {
                // Find the common ancestor in this pair of revisions on the common ancestor branch
                // If the common ancestor cannot be found, use the lowest numbered revision
                // TODO(mjansson): By keeping branch epochs and sequentially force push, we can
                //                 avoid trying to detect divergence here if branch points are known
                //                 to be from the same epoch
                base_revision = Box::pin(find_divergence_base(
                    repository.clone(),
                    branch_source_point,
                    branch_target_point,
                    base_branch_point,
                ))
                .await?
                .base_revision;
            } else {
                base_revision = branch_source_point;
            }

            if let Some(found_base_revision) = find_ancestor_revision(
                repository.clone(),
                source_branch,
                source_revision,
                target_branch,
                target_revision,
                base_revision,
            )
            .await
            {
                base_revision = found_base_revision;
                lore_debug!(
                    "Found new base revision from previous merges from source branch, using branch point {base_revision}"
                );
            } else {
                lore_debug!(
                    "No new base revision from previous merges from source branch found, using branch point {base_revision}"
                );
            }
        }
    } else {
        let base_branch_point = metadata(repository.clone(), source_branch)
            .await
            .and_then(|metadata| {
                let stack = stack(&metadata);
                lore_debug!(
                    "Loaded local metadata for source branch {source_branch}, found stack {stack:?}"
                );
                stack
                    .first()
                    .map(|parent| parent.revision)
                    .ok_or_else(|| BranchError::internal("Invalid parent"))
            })
            .unwrap_or_default();

        base_revision = Box::pin(find_divergence_base(
            repository.clone(),
            source_revision,
            target_revision,
            base_branch_point,
        ))
        .await?
        .base_revision;
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
            tx,
        )
    })
    .await?;
    Ok(revision::diff_result_from_summary_and_items(summary, items))
}

#[derive(Debug)]
pub struct RevisionDivergence {
    pub base_revision: Hash,
    pub self_distance: usize,
    pub other_distance: usize,
}

pub async fn find_divergence_base(
    repository: Arc<RepositoryContext>,
    self_revision: Hash,
    other_revision: Hash,
    base_revision: Hash,
) -> Result<RevisionDivergence, BranchError> {
    lore_debug!(
        "Find base revision from {self_revision} and {other_revision} with given base {base_revision}"
    );

    let base_revision_number = State::deserialize(repository.clone(), base_revision)
        .await
        .forward::<BranchError>("Failed to deserialize base revision state")?
        .revision_number();

    let mut source_reached_base = false;
    let mut target_reached_base = false;

    lore_debug!("Batch fetch history from {self_revision}");
    let mut source_revisions = find::batch_load_history(repository.clone(), self_revision).await;
    if source_revisions.is_empty() {
        // Try walking history locally
        let mut load_count = 0;
        if let Ok(mut state_iter) =
            state::State::deserialize(repository.clone(), self_revision).await
        {
            while !state_iter.parent_self().is_zero()
                && state_iter.parent_self() != base_revision
                && load_count < 100
                && !source_reached_base
            {
                let revision_next = state_iter.parent_self();
                source_revisions.push(revision_next);
                if let Ok(state_next) =
                    state::State::deserialize(repository.clone(), revision_next).await
                {
                    if state_next.revision_number() <= base_revision_number {
                        source_reached_base = true;
                    }
                    state_iter = state_next;
                    load_count += 1;
                } else {
                    source_reached_base = true;
                }
            }
        }
    }

    lore_debug!("Batch fetch history from {other_revision}");
    let mut target_revisions = find::batch_load_history(repository.clone(), other_revision).await;
    if target_revisions.is_empty() {
        // Try walking history locally
        lore_debug!("Found no revision from target, local walk");
        let mut load_count = 0;
        if let Ok(mut state_iter) =
            state::State::deserialize(repository.clone(), other_revision).await
        {
            while !state_iter.parent_self().is_zero()
                && state_iter.parent_self() != base_revision
                && load_count < 100
                && !target_reached_base
            {
                let revision_next = state_iter.parent_self();
                target_revisions.push(revision_next);
                if let Ok(state_next) =
                    state::State::deserialize(repository.clone(), revision_next).await
                {
                    if state_next.revision_number() <= base_revision_number {
                        target_reached_base = true;
                    }
                    state_iter = state_next;
                    load_count += 1;
                } else {
                    target_reached_base = true;
                }
            }
        }
    }

    loop {
        for (source_count, source) in source_revisions.iter().enumerate() {
            for (target_count, target) in target_revisions.iter().enumerate() {
                lore_trace!(
                    "Check source revision {} against target revision {}",
                    *source,
                    *target
                );
                if *source == *target {
                    let divergence = RevisionDivergence {
                        base_revision: *source,
                        self_distance: source_count,
                        other_distance: target_count,
                    };
                    lore_debug!("Found base revision {divergence:?}");
                    return Ok(divergence);
                }
                if *target == base_revision || target.is_zero() {
                    target_reached_base = true;
                    break;
                }
            }

            if *source == base_revision || source.is_zero() {
                source_reached_base = true;
                break;
            }

            if source_reached_base && target_reached_base {
                break;
            }
        }

        if source_reached_base && target_reached_base {
            lore_debug!("Both history lines reached base revision or revision number");
            return Ok(RevisionDivergence {
                base_revision,
                self_distance: source_revisions.len(),
                other_distance: target_revisions.len(),
            });
        }

        // Exit condition - too many iterations
        let can_fetch_more_source =
            source_revisions.len() < MAX_DIVERGENT_HISTORY_LENGTH && !source_reached_base;
        let can_fetch_more_target =
            target_revisions.len() < MAX_DIVERGENT_HISTORY_LENGTH && !target_reached_base;
        if !can_fetch_more_source && !can_fetch_more_target {
            lore_warn!(
                "Reached maximum history depth of {MAX_DIVERGENT_HISTORY_LENGTH} without finding common base revision, fall back to common branch point {base_revision}"
            );
            return Ok(RevisionDivergence {
                base_revision,
                self_distance: 0,
                other_distance: 0,
            });
        }

        if !source_reached_base {
            source_reached_base = load_additional_history(
                repository.clone(),
                &mut source_revisions,
                base_revision,
                base_revision_number,
            )
            .await;
        }

        if !target_reached_base {
            target_reached_base = load_additional_history(
                repository.clone(),
                &mut target_revisions,
                base_revision,
                base_revision_number,
            )
            .await;
        }
    }
}

async fn load_additional_history(
    repository: Arc<RepositoryContext>,
    history: &mut Vec<Hash>,
    base_revision: Hash,
    base_revision_number: u64,
) -> bool {
    let mut additional = find::batch_load_history(
        repository.clone(),
        if let Some(last) = history.last() {
            *last
        } else {
            base_revision
        },
    )
    .await;

    if additional.is_empty() {
        return true;
    }

    let Ok(state) = State::deserialize(repository.clone(), *additional.last().unwrap()).await
    else {
        return true;
    };

    if state.revision_number() < base_revision_number {
        return true;
    }

    history.append(&mut additional);

    false
}

struct WalkVisit {
    source: bool,
    target: bool,
}

async fn find_ancestor_walker(
    repository: Arc<RepositoryContext>,
    revision_start: Hash,
    revision_stop: Hash,
    visited: Arc<DashMap<Hash, WalkVisit>>,
    common: Arc<RwLock<Option<(u64, Hash)>>>,
    is_target: bool,
) {
    let mut tasks = JoinSet::new();

    let mut revision = revision_start;
    while revision != Hash::default() {
        if let Ok(state) = state::State::deserialize(repository.clone(), revision).await {
            // Update bookkeeping on revision visits.
            let both_visited = match visited.entry(revision) {
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

            // Update bookkeeping when encountering a common ancestor with a higher revision number.
            if both_visited {
                let mut common = common.write().await;

                if let Some((found_revision_number, _found_revision)) = *common {
                    if state_revision_number > found_revision_number {
                        *common = Some((state_revision_number, state_revision));
                    }
                } else {
                    *common = Some((state_revision_number, state_revision));
                }

                // If revision has been visited by both the target and source, iteration can stop.
                break;
            }

            // If a common ancestor with a higher revision number was already found, iteration can stop.
            {
                if let Some((found_revision_number, _found_revision)) = *common.read().await
                    && state_revision_number <= found_revision_number
                {
                    break;
                }
            }

            // If revision equals the stop revision, iteration can stop.
            if revision == revision_stop {
                break;
            }

            // Walk other parent, if this is a merge.
            let parent_other = state.parent_other();
            if parent_other != Hash::default() {
                lore_spawn!(tasks, {
                    let repository = repository.clone();
                    let visited = visited.clone();
                    let common = common.clone();
                    async move {
                        find_ancestor_walker_recurse(
                            repository,
                            parent_other,
                            revision_stop,
                            visited,
                            common,
                            is_target,
                        )
                        .await;
                    }
                });
            }

            // Walk self parent.
            revision = state.parent_self();
        } else {
            lore_warn!(
                "Could not deserialize state for {} - aborting walk",
                revision
            );
            break;
        }
    }

    while let Some(_result) = tasks.join_next().await {}
}

fn find_ancestor_walker_recurse(
    repository: Arc<RepositoryContext>,
    revision_start: Hash,
    revision_stop: Hash,
    visited: Arc<DashMap<Hash, WalkVisit>>,
    common: Arc<RwLock<Option<(u64, Hash)>>>,
    is_target: bool,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(find_ancestor_walker(
        repository,
        revision_start,
        revision_stop,
        visited,
        common,
        is_target,
    ))
}

async fn find_ancestor_revision(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source_revision: Hash,
    target_branch: BranchId,
    target_revision: Hash,
    base_revision: Hash,
) -> Option<Hash> {
    let mut tasks = JoinSet::new();

    // Bookkeeping to hold if a revision has been visited by the source walk and/or the target walk.
    let visited: Arc<DashMap<Hash, WalkVisit>> = Arc::new(DashMap::new());

    // Bookkeeping to hold the common ancestor with the highest revision number.
    let base_revision_number = State::deserialize(repository.clone(), base_revision)
        .await
        .map(|state| state.revision_number())
        .unwrap_or_default();
    let common: Arc<RwLock<Option<(u64, Hash)>>> =
        Arc::new(RwLock::new(Some((base_revision_number, base_revision))));

    // Start walking source.
    lore_spawn!(tasks, {
        let repository = repository.clone();
        let visited = visited.clone();
        let common = common.clone();
        let is_target = false;
        async move {
            lore_debug!(
                "Walking backwards on source branch {source_branch} from {source_revision}"
            );

            find_ancestor_walker(
                repository,
                source_revision,
                base_revision,
                visited,
                common,
                is_target,
            )
            .await;
        }
    });

    // Start walking target.
    lore_spawn!(tasks, {
        let repository = repository.clone();
        let visited = visited.clone();
        let common = common.clone();
        let is_target = true;
        async move {
            lore_debug!(
                "Walking backwards on target branch {target_branch} from {target_revision}"
            );

            find_ancestor_walker(
                repository,
                target_revision,
                base_revision,
                visited,
                common,
                is_target,
            )
            .await;
        }
    });

    // Wait until both walks are finished.
    while let Some(_result) = tasks.join_next().await {}

    // Process results.
    if let Some((revision_number, revision)) = *common.read().await {
        lore_debug!(
            "Revision {} -> {} found as common ancestor",
            revision,
            revision_number
        );

        Some(revision)
    } else {
        lore_debug!(
            "Revision {} used as common ancestor because walk found no result",
            base_revision
        );

        Some(base_revision)
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
