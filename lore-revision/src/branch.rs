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
use lore_base::text::fold_eq;
use lore_base::types::BranchMetadata;
use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;
use lore_transport::BranchRecord;
use lore_transport::Connection;
use lore_transport::MatchedProtocolError;
use lore_transport::ProtocolError;
use serde::Serialize;
use tokio::join;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::try_join;
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
use crate::history;
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchArchiveEventData {
    /// Name of the archived branch.
    pub name: LoreString,
}

/// Event data reported at the start of a branch listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchListBeginEventData {
    /// Location the listed branches come from.
    pub location: LoreBranchLocation,
}

/// Event data reported for each branch in a branch listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchListEndEventData {
    /// Location the listed branches came from.
    pub location: LoreBranchLocation,
    /// Number of branches that were listed.
    pub count: u64,
}

/// Event data reported at the start of a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffBeginEventData {
    /// Identifier of the source branch of the diff.
    pub source_branch: BranchId,
    /// Name of the source branch.
    pub source_branch_name: LoreString,
    /// Revision of the source branch used in the diff.
    pub source_revision: Hash,
    /// Identifier of the target branch of the diff.
    pub target_branch: BranchId,
    /// Name of the target branch.
    pub target_branch_name: LoreString,
    /// Revision of the target branch used in the diff.
    pub target_revision: Hash,
    /// Base revision the 3-way diff was resolved against.
    pub base_revision: Hash,
}

/// Event data describing a single changed node in a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
    #[lore_macro::test_pub]
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
            path: display_path(node_change.path().as_str()),
            automerged: node_change.flags.is_conflict_automerged().into(),
            from_path: node_change
                .move_source()
                .map(|path| display_path(path.as_str()))
                .unwrap_or_default(),
        }
    }
}

/// Event data reported at the start of the change section of a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffChangeBeginEventData {
    /// Number of changes that follow.
    pub changes_count: usize,
}

/// Event data reporting a single change in a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffChangeEventData {
    /// The changed node.
    pub change: LoreBranchDiffNodeData,
}

/// Event data reported at the end of the change section of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffChangeEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported at the start of the conflict section of a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffConflictBeginEventData {
    /// Number of conflicts that follow.
    pub conflicts_count: usize,
}

/// Event data reporting a single conflict in a branch diff.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffConflictEventData {
    /// The change on the source side of the conflict.
    pub source_change: LoreBranchDiffNodeData,
    /// The change on the target side of the conflict.
    pub target_change: LoreBranchDiffNodeData,
}

/// Event data reported at the end of the conflict section of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffConflictEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported at the end of a branch diff.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchDiffEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported when a branch is protected.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchProtectEventData {
    /// Name of the protected branch.
    pub name: LoreString,
}

/// Event data reported when a branch is unprotected.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
pub const REVISION_NUMBER_STEP: &str = "branch-revision-number-step-v2";
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

/// "functions" passed to `mutable_key_type` that may exist in the Mutable Store that are no longer
/// referenced by the codebase, and can be removed without data loss.
///
/// These are not guaranteed to exist and depend on the versions of `lore-server` that have been
/// used against the Mutable Store
pub const ORPHANED_MUTABLE_STORE_KEY_TYPE_FUNCTIONS: [&str; 1] = [
    // a revision step acceleration key, that had a bug which meant step boundaries were prematurely
    // sealed and legitimate revisions could not be found in boundaries where they should have been
    "branch-revision-number-step",
];

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
/// The time of the branch's last restore, in milliseconds since the Unix epoch, which an archive
/// removes.
pub const RESTORED: &str = "restored";
/// The time of the branch's last archive, in milliseconds since the Unix epoch. An archive and a
/// settle set it. A restore's first swap sets it where the metadata doesn't say the branch is
/// archived, and keeps it otherwise.
pub const ARCHIVED: &str = "archived";
/// The name a restore of the archived branch claims, set before the claim and removed when the
/// restore finishes: a restore in progress, or one that stopped.
pub const RESTORING: &str = "restoring";
/// The name a settled restore had claimed, which may still map to the branch: every archive of the
/// branch releases it again, and the next restore releases it and removes the entry before it
/// records itself.
pub const RELEASED: &str = "released";
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
    // Saturating: a revision number within a step of `u64::MAX` cannot exist, so the
    // clamped bucket is a key that never matches rather than an overflow panic on a
    // number taken straight from a request.
    let key_revision_number = revision_number
        .div_ceil(step_size)
        .saturating_mul(step_size);
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
    // Saturating: a revision number within a step of `u64::MAX` cannot exist, so the
    // clamped bucket is a key that never matches rather than an overflow panic on a
    // number taken straight from a request.
    let key_revision_number = revision_number
        .div_ceil(step_size)
        .saturating_mul(step_size);
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

/// The branch id derived from `name`: the first 16 bytes of the hash of `name` as given. The
/// name's mapping key lowercases the name, so `Main` and `main` share a mapping but derive
/// different ids.
#[lore_macro::test_pub]
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

/// Swaps the `function` value of `branch` from `expect` to `value`, answering the value it held.
#[inline]
pub fn mutable_try_store(
    repository: Arc<RepositoryContext>,
    function: &str,
    branch: BranchId,
    expect: Hash,
    value: Hash,
) -> impl Future<Output = Result<Hash, BranchError>> + use<> {
    let (key, key_type) = mutable_key(repository.salt(), function, repository.id, branch);
    lore_debug!("Store {function} = {value} for branch {branch}");
    mutable_try_store_key(repository, key, key_type, expect, value)
}

/// Swaps the value of `key` from `expected` to `value`, answering the value the key held. An
/// absent key holds zero.
async fn mutable_try_store_key(
    repository: Arc<RepositoryContext>,
    key: Hash,
    key_type: KeyType,
    expected: Hash,
    value: Hash,
) -> Result<Hash, BranchError> {
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| BranchError::from(WriteRequired))?;
    handle
        .compare_and_swap(repository.id, key, expected, value, key_type)
        .await
        .forward_with::<BranchError, _>(|| format!("compare-and-swap of a {key_type:?} key"))
}

/// Claims `name` for branch `id` with a compare-and-swap of the name's mapping from `expected`,
/// the value the caller read: zero for an absent mapping or an archived name. The claim succeeds
/// when the mapping holds `expected` or already names `id`. A mapping that holds anything else is
/// left as it is, and the claim fails with [`BranchAlreadyExists`]. A zero `id` fails with
/// [`InvalidArguments`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn claim_name(
    repository: Arc<RepositoryContext>,
    name: &str,
    expected: Context,
    id: BranchId,
) -> impl Future<Output = Result<(), BranchError>> {
    async move {
        let held = swap_claim(repository, name, expected, id)?
            .await?
            .to_context();
        if held == expected || held == id {
            return Ok(());
        }
        lore_debug!("Name {name} maps to {held}, not {expected} or {id}, so it stays unchanged");
        Err(BranchError::from(BranchAlreadyExists {
            branch: name.to_string(),
        }))
    }
}

/// The swap of the mapping of `name` from `expected` to `id`, which answers the value the mapping
/// held, zero for none. A zero `id` fails with [`InvalidArguments`] before the swap starts. A plain
/// `fn` answering the swap's own future, so a caller awaiting it holds its arguments once.
fn swap_claim(
    repository: Arc<RepositoryContext>,
    name: &str,
    expected: Context,
    id: BranchId,
) -> Result<impl Future<Output = Result<Hash, BranchError>> + use<>, BranchError> {
    if id.is_zero() {
        return Err(BranchError::from(InvalidArguments {
            reason: format!("the branch id claiming the name {name} is zero"),
        }));
    }
    Ok(swap_name_to_id(
        repository,
        name,
        Hash::from_context(expected),
        Hash::from_context(id),
    ))
}

/// Releases `name` from branch `expected` with a compare-and-swap of the name's mapping to zero. A
/// mapping that names another branch, or none, is left as it is, and the release fails with
/// [`BranchNotFound`], as it does for a zero `expected`.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn release_name(
    repository: Arc<RepositoryContext>,
    name: &str,
    expected: BranchId,
) -> impl Future<Output = Result<(), BranchError>> {
    async move {
        if !expected.is_zero() {
            let held = swap_name_to_id(
                repository,
                name,
                Hash::from_context(expected),
                Hash::default(),
            )
            .await?;
            if held == Hash::from_context(expected) {
                return Ok(());
            }
            lore_debug!(
                "Name {name} maps to {}, not {expected}, so it stays unchanged",
                held.to_context()
            );
        }
        Err(BranchError::from(BranchNotFound {
            branch: name.to_string(),
        }))
    }
}

/// Claims `name` for `branch` unless another branch holds it, answering whether the name maps to
/// `branch`. A mapping that names another branch with metadata is left as it is, as is one another
/// writer changes between the read and the claim, other than to `branch`, and both answer `false`.
/// A mapping to an id without metadata is stale, and the claim replaces it. A read that fails
/// other than as not found fails.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn claim_name_unless_held(
    repository: Arc<RepositoryContext>,
    name: &str,
    branch: BranchId,
) -> impl Future<Output = Result<bool, BranchError>> {
    async move {
        let claimed = match unclaimed_name_mapping(repository.clone(), name, branch).await {
            Ok(expected) => claim_name(repository, name, expected, branch).await,
            Err(err) => Err(err),
        };
        match claimed {
            Ok(()) => Ok(true),
            Err(err) if err.is_branch_already_exists() => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Swaps the mapping of `name` from `expected` to `value`, answering the value the mapping held.
/// An absent mapping holds zero.
fn swap_name_to_id(
    repository: Arc<RepositoryContext>,
    name: &str,
    expected: Hash,
    value: Hash,
) -> impl Future<Output = Result<Hash, BranchError>> + use<> {
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name);
    mutable_try_store_key(repository, key, key_type, expected, value)
}

/// The id `name` maps to: `name` itself when it parses as a non-zero id, else the mapping in the
/// local store, then the remote's, then the id derived from `name` when the local store holds a
/// latest revision for it. A failed read of the local mapping counts as none. A remote that refuses
/// the caller fails the lookup, and a name found nowhere answers `BranchNotFound`. A mapping found
/// at the remote, or a derived one, is cached as `load_name_to_id_unmapped` describes.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn load_name_to_id(
    repository: Arc<RepositoryContext>,
    name: impl AsRef<str>,
) -> impl Future<Output = Result<Context, BranchError>> {
    async move {
        if let Ok(id) = Context::from_str(name.as_ref())
            && !id.is_zero()
        {
            return Ok(id);
        }

        if let Ok(id) = load_name_to_id_local(repository.clone(), name.as_ref()).await {
            return Ok(id);
        }

        load_name_to_id_unmapped(repository, name.as_ref()).await
    }
}

/// The id `name` maps to where the local store holds no mapping for it: the remote's mapping, then
/// the id derived from `name` when the local store holds a latest revision for it. A remote that
/// refuses the caller fails the lookup, and a name found at neither answers `BranchNotFound`.
///
/// A mapping found at the remote, or a derived one, is cached in the local store with a claim from
/// none, so a mapping another writer made after the caller's read stays. The remote's mapping is
/// claimed only after the remote's metadata of its branch is cached ([`cache_remote_metadata`]). A
/// failed write is ignored.
///
/// Of the remote's record it keeps only the id and metadata across its awaits, and of the
/// fallback's answer only the id.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_name_to_id_unmapped(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> impl Future<Output = Result<Context, BranchError>> {
    async move {
        if let Some(remote) = absent_unless_refused::<_, _, BranchError>(
            repository.remote().await,
            "connecting to the remote",
        )? && let Some(revision_service) = absent_unless_refused::<_, _, BranchError>(
            remote.revision(repository.id).await,
            "connecting to the remote revision service",
        )? && let Some((branch, metadata)) = absent_unless_refused::<_, _, BranchError>(
            revision_service
                .branch_query(None, Some(name))
                .await
                .map(|record| (record.id, record.metadata)),
            "querying the remote for the branch name",
        )? && !branch.is_zero()
        {
            if cache_remote_metadata(repository.clone(), branch, metadata)
                .await
                .is_ok()
            {
                let _ = claim_name(repository.clone(), name, Context::default(), branch).await;
            }
            return Ok(branch);
        }

        if let Some(id) = live_fallback_id(repository.clone(), name)
            .await
            .ok()
            .flatten()
        {
            let _ = claim_name(repository.clone(), name, Context::default(), id).await;
            return Ok(id);
        }

        Err(BranchError::from(BranchNotFound {
            branch: name.to_string(),
        }))
    }
}

/// Looks `name`, the name of `branch`, up as [`load_name_to_id`] does, which caches in the local
/// store a mapping only the remote holds, with the remote's metadata: a live branch the local store
/// hadn't mapped then lists locally, as a lookup by its name leaves it, and an archived one, which
/// the remote maps no name to, stays archived. A lookup that fails or finds another branch is
/// logged and changes nothing else.
pub(crate) async fn cache_name_mapping(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
) {
    match load_name_to_id(repository, name).await {
        Ok(held) if held == branch => {}
        Ok(held) => lore_debug!("Branch name {name} maps to {held}, not {branch}"),
        Err(err) => lore_debug!("Failed to look up the name {name} of branch {branch}: {err}"),
    }
}

/// The id derived from `name`, when the local store holds a latest revision for that id.
async fn live_fallback_id(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Option<BranchId>, BranchError> {
    let id = fallback_id(name);
    if id.is_zero() {
        return Ok(None);
    }
    match load_latest_local(repository, id).await {
        Ok(_) => Ok(Some(id)),
        Err(err) if err.is_branch_not_found() => Ok(None),
        Err(err) => Err(err),
    }
}

/// The branch a name names in the local store, by its mapping or by the id derived from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedBranch {
    /// The name's mapping holds the id.
    Mapped(BranchId),
    /// The name has no mapping, or an entry holding zero that an older version wrote, and the id
    /// derived from the name has a latest revision.
    Derived(BranchId),
}

impl NamedBranch {
    /// The branch the name names.
    pub fn branch(self) -> BranchId {
        match self {
            Self::Mapped(id) | Self::Derived(id) => id,
        }
    }

    /// The value the name's mapping holds, zero for a derived id: the `expected` value of a
    /// [`claim_name`] of the name.
    pub fn mapping(self) -> Context {
        match self {
            Self::Mapped(id) => id,
            Self::Derived(_) => Context::default(),
        }
    }
}

/// Looks up the branch `name` names in the local store alone, writing nothing: the id the name's
/// mapping holds or, for a name with no mapping (an archived name included), the id derived from
/// the name when that id has a latest revision. `None` when neither names a branch. A failed read
/// is an error, not `None`.
pub async fn lookup_name_local_unwritten(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Option<NamedBranch>, BranchError> {
    // A block of its own, so the future holds no mapping while it reads the derived id.
    {
        let held = local_name_mapping(repository.clone(), name).await?;
        if !held.is_zero() {
            return Ok(Some(NamedBranch::Mapped(held)));
        }
    }
    Ok(live_fallback_id(repository, name)
        .await?
        .map(NamedBranch::Derived))
}

/// The value the mapping of `name` holds in the local store, zero for none. A read that fails other
/// than as not found fails.
///
/// Not an `async fn`, which would hold a second copy of its arguments beside the read's future.
fn local_name_mapping(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> impl Future<Output = Result<Context, BranchError>> {
    futures::FutureExt::map(load_name_to_id_local(repository, name), |read| match read {
        Err(err) if err.is_branch_not_found() => Ok(Context::default()),
        read => read,
    })
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
            match metadata_hash(repository, branch).await {
                Ok(_metadata) => Ok(Hash::default()),
                Err(refused) if is_refusal(&refused) => Err(refused),
                Err(_) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

/// Reads the latest revision of `branch` from the mutable store alone, reporting an absent value
/// as `BranchNotFound` even for a branch that exists.
pub fn load_latest_local(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> impl Future<Output = Result<Hash, BranchError>> {
    mutable_load(repository, LATEST, branch)
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
        && let Some(parent) = branch_metadata.branch_point()
    {
        Ok(parent.revision)
    } else {
        Err(BranchError::from(BranchNotFound {
            branch: branch.to_string(),
        }))
    }
}

/// The latest revision of `branch` on the remote. A failed branch query, other than a refusal, is
/// reported as [`BranchNotFound`].
pub async fn load_remote_latest(
    remote: Arc<Connection>,
    repository: RepositoryId,
    branch: BranchId,
) -> Result<Hash, BranchError> {
    remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?
        .branch_query(Some(branch), None)
        .await
        .map(|response| response.latest)
        .map_err(|err| {
            forward_refusal_or(err, "querying the remote branch", |_err| {
                BranchError::from(BranchNotFound {
                    branch: branch.to_string(),
                })
            })
        })
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

/// The metadata hash of `branch`, as [`load_local_or_remote`] reads it.
pub fn metadata_hash(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> impl Future<Output = Result<Hash, BranchError>> {
    futures::FutureExt::map(
        load_local_or_remote(repository, BranchValue::Metadata, branch),
        |read| read.map(|(hash, _local)| hash),
    )
}

/// A value of a branch [`load_local_or_remote`] reads: one byte, so a future reading it holds no
/// key name or function pointer across its awaits.
#[derive(Clone, Copy)]
enum BranchValue {
    Metadata,
    Latest,
}

impl BranchValue {
    /// The function of the value's key in the mutable store.
    fn function(self) -> &'static str {
        match self {
            Self::Metadata => METADATA,
            Self::Latest => LATEST,
        }
    }

    /// The value in the remote's status of the branch.
    fn remote(self, status: &BranchStatus) -> Hash {
        match self {
            Self::Metadata => status.metadata,
            Self::Latest => status.latest,
        }
    }
}

/// The `value` of `branch` in the local store and `true`, or where the local store holds none it
/// can read, the non-zero value the remote's status of the branch holds and `false`. Only a
/// refusal by the remote fails in place of the local read: no remote, one that can't be reached,
/// and one without the value answer the local read's error.
async fn load_local_or_remote(
    repository: Arc<RepositoryContext>,
    value: BranchValue,
    branch: BranchId,
) -> Result<(Hash, bool), BranchError> {
    let result = mutable_load(repository.clone(), value.function(), branch).await;
    if let Ok(local) = result {
        return Ok((local, true));
    }

    if let Some(remote) = absent_unless_refused::<_, _, BranchError>(
        repository.remote().await,
        "connecting to the remote",
    )? && let Some(status) = absent_unless_refused::<_, _, BranchError>(
        load_remote(remote, repository.id, branch).await,
        "loading the remote branch",
    )? && !value.remote(&status).is_zero()
    {
        return Ok((value.remote(&status), false));
    }

    result.map(|local| (local, true))
}

/// Stores `hash` as the metadata hash of `branch` in the local mutable store, over whatever the key
/// holds. Tests only: every other metadata write is a compare-and-swap.
#[lore_macro::test_pub]
#[allow(dead_code)]
async fn mutable_store_metadata(
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
/// branch. An archive releases the mapping with a compare-and-swap to zero
/// ([`archive_record`]), which removes it, so a branch archived locally is
/// observable here as a mapping that names another branch, or none. Returns
/// `false` when the metadata can't be deserialized or has no name — defensive
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
    let remote = repository
        .remote()
        .await
        .map_err(|err| remote_lookup_failure(err, branch))?;
    let service = remote
        .revision(repository.id)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;
    let response = if let Ok(id) = Context::from_str(branch) {
        service.branch_query(Some(id), None).await
    } else {
        service.branch_query(None, Some(branch)).await
    };
    queried_status(response, branch)
}

/// `err`, the failed connect of a lookup of `branch` on the remote, as the lookup's failure. A
/// refusal of the credentials passes on; any other failure is `BranchNotFound`, as the branch is
/// unknown without the remote.
fn remote_lookup_failure(err: ProtocolError, branch: &str) -> BranchError {
    if is_refusal(&err) {
        return err.forward("connecting to remote for branch lookup");
    }
    BranchError::BranchNotFound(
        BranchNotFound {
            branch: branch.to_string(),
        }
        .chain_err_from(err, "remote unavailable for branch lookup"),
    )
}

/// The status of `branch` from the remote's `response` to a query for it. A branch the remote does
/// not hold is `BranchNotFound`, a refusal of the credentials passes on, and any other failure is
/// internal.
fn queried_status(
    response: Result<lore_transport::BranchQueryResponse, ProtocolError>,
    branch: &str,
) -> Result<BranchStatus, BranchError> {
    match response {
        Ok(response) => Ok(BranchStatus {
            id: response.id,
            latest: response.latest,
            metadata: response.metadata,
            local: false,
            deleted: response.deleted,
        }),
        Err(ProtocolError::NotFound(_)) => Err(BranchError::from(BranchNotFound {
            branch: branch.to_string(),
        })),
        Err(err) if is_refusal(&err) => Err(err.forward("querying branch on remote")),
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
    let id = if let Ok(id) = Context::from_str(branch) {
        id
    } else if let Some(id) = absent_unless_refused::<_, _, BranchError>(
        branch::load_name_to_id(repository.clone(), branch).await,
        "looking up the branch name",
    )? {
        id
    } else {
        let remote = repository
            .remote()
            .await
            .map_err(|err| remote_lookup_failure(err, branch))?;
        let response = remote
            .revision(repository.id)
            .await
            .forward::<BranchError>("Failed to connect to remote revision service")?
            .branch_query(None, Some(branch))
            .await;
        return queried_status(response, branch);
    };

    if let Some(metadata) = absent_unless_refused::<_, _, BranchError>(
        metadata_hash(repository.clone(), id).await,
        "loading the branch metadata hash",
    )? {
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

    if let Some(remote) = absent_unless_refused::<_, _, BranchError>(
        repository.remote().await,
        "connecting to the remote",
    )? && let Some(status) = absent_unless_refused::<_, _, BranchError>(
        load_remote(remote, repository.id, id).await,
        "loading the remote branch",
    )? {
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

/// The metadata of `branch`, read locally, or on the remote where the instance holds none it can
/// read. Keeping the remote's metadata hash locally is best effort, a compare-and-swap from the
/// value the local read found, so a context that cannot write still reads the remote's metadata,
/// and metadata another writer stored locally since that read stays. The remote's refusal of the
/// caller is returned.
pub async fn metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Metadata, BranchError> {
    let (local_metadata, expected) =
        match load_local_or_remote(repository.clone(), BranchValue::Metadata, branch).await {
            Ok((hash, local)) => {
                let expected = if local { hash } else { Hash::default() };
                (load_metadata(repository.clone(), hash).await, expected)
            }
            Err(err) => (Err(err), Hash::default()),
        };
    match local_metadata {
        Err(err) if !is_refusal(&err) => {
            Box::pin(remote_metadata(repository, branch, expected, err)).await
        }
        local_metadata => local_metadata,
    }
}

/// The remote's metadata of `branch`, for a [`metadata`] read whose local read failed with
/// `local`, which it answers where there is no remote or the remote doesn't hold the branch. It
/// keeps the remote's metadata hash in the local metadata key, best effort, with a
/// compare-and-swap from `expected`.
///
/// A cold path, which [`metadata`] boxes: it runs only where the local store holds no metadata of
/// the branch it can read. Inline, it would make every future awaiting a metadata read hold the
/// remote's round trip and `local`.
async fn remote_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    expected: Hash,
    local: BranchError,
) -> Result<Metadata, BranchError> {
    if let Some(remote) = absent_unless_refused::<_, _, BranchError>(
        repository.remote().await,
        "connecting to the remote",
    )? && let Some(status) = absent_unless_refused::<_, _, BranchError>(
        load_remote(remote, repository.id, branch).await,
        "loading the remote branch",
    )? {
        let _ = mutable_try_store(
            repository.clone(),
            METADATA,
            branch,
            expected,
            status.metadata,
        )
        .await;
        return load_metadata(repository, status.metadata).await;
    }

    Err(local)
}

pub async fn metadata_local(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Metadata, BranchError> {
    let hash = metadata_hash(repository.clone(), branch).await?;
    load_metadata(repository, hash).await
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
/// observe a complete record.
fn patch_missing_metadata_fields(
    metadata: &mut Metadata,
    category: &str,
    creator: &str,
    created: u64,
) -> Result<(), BranchError> {
    if metadata.get_string(CATEGORY).is_err() {
        metadata
            .set_string(CATEGORY, category)
            .forward::<BranchError>("Failed to patch CATEGORY metadata")?;
    }
    if metadata.get_string(CREATOR).is_err() {
        metadata
            .set_string(CREATOR, creator)
            .forward::<BranchError>("Failed to patch CREATOR metadata")?;
    }
    if metadata.get_u64(CREATED).is_err() {
        metadata
            .set_u64(CREATED, created)
            .forward::<BranchError>("Failed to patch CREATED metadata")?;
    }
    Ok(())
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

/// [`create_at`], returning the branch it made.
///
/// Not an `async fn`, which would hold a second copy of its arguments beside the future of
/// [`create_at`]. An `async` block awaiting that future would hold it twice.
#[allow(clippy::too_many_arguments)]
pub fn create(
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
) -> impl Future<Output = Result<Context, BranchError>> {
    futures::FutureExt::map(
        create_at(
            repository,
            token,
            branch,
            name,
            category,
            creator,
            created,
            stack,
            dry_run,
            create_linked,
        ),
        |created| created.map(|created| created.branch),
    )
}

/// A branch [`create_at`] made, and where it left the instance.
pub struct CreatedBranch {
    pub branch: BranchId,
    /// The latest revision stored for the branch. Zero where an existing branch was restored.
    pub head: Hash,
    /// The staged state based on `head`, where linked branches repinned or rebased it.
    pub staged: Option<Hash>,
}

/// [`create`], reporting the head it stored and the staged state rebased onto it.
///
/// The steps, in order:
/// 1. Restore `branch` where it has metadata (`restore_existing_branch`), which reads the value
///    its claim of `name` swaps from itself, and may settle a pending restore of the branch, or
///    release a name its metadata marks released, before a check refuses it.
/// 2. Otherwise find the value the claim of `name` swaps from (`name_claim_expected`). A branch the
///    name names with metadata refuses the create.
/// 3. Check the parent and, outside a dry run, create the linked branches and write the record
///    (`create_record`), claiming the name from that value.
///
/// A name that isn't valid, or a zero `branch`, fails with [`InvalidArguments`] before anything is
/// written. In step 3, a parent that is zero, the branch itself, or a personal branch, or a zero
/// parent revision on a branch other than the default branch, fails with [`InvalidArguments`] too.
#[allow(clippy::too_many_arguments)]
pub async fn create_at(
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
) -> Result<CreatedBranch, BranchError> {
    // Check if name is valid
    if !is_valid_name(name) {
        return Err(BranchError::from(InvalidArguments {
            reason: format!("the branch name {name:?} is not valid"),
        }));
    }
    check_new_branch_id(branch, name)?;

    if restore_existing_branch(
        repository.clone(),
        branch,
        name,
        category,
        creator,
        created,
        dry_run,
    )
    .await?
    {
        return Ok(CreatedBranch {
            branch,
            head: Hash::default(),
            staged: None,
        });
    }

    let name_expected = name_claim_expected(repository.clone(), name, BranchId::default()).await?;
    #[cfg(feature = "test-util")]
    crate::test_util::branch_step(crate::test_util::BranchStep::CreateNameRead).await;

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
                    return Err(BranchError::from(InvalidArguments {
                        reason: "the parent branch id is zero".to_string(),
                    }));
                }
                if parent.branch == branch {
                    lore_error!("Branch cannot have itself as parent");
                    return Err(BranchError::from(InvalidArguments {
                        reason: format!("branch {branch} is its own parent"),
                    }));
                }

                if let Ok(parent_metadata) =
                    branch::metadata(repository.clone(), parent.branch).await
                {
                    let parent_category =
                        branch::category(&parent_metadata).unwrap_or(branch::default_category());
                    if parent_category == branch::personal_category() {
                        lore_error!("Branch cannot have a personal branch as parent");
                        return Err(BranchError::from(InvalidArguments {
                            reason: format!("the parent branch {} is personal", parent.branch),
                        }));
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
                        return Err(BranchError::from(InvalidArguments {
                            reason: format!(
                                "the parent revision is zero on {}, which isn't the default branch",
                                parent.branch
                            ),
                        }));
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
    let mut staged = None;

    if !dry_run {
        lore_debug!("Creating branch {name} {branch} with stack {stack:?} at signature {head}");

        if !head.is_zero() && create_linked {
            if let Ok((state_current, state_staged, current_branch)) =
                State::deserialize_current_and_staged(repository.clone())
                    .await
                    .forward::<BranchError>("Failed to deserialize current revision anchor")
            {
                let (serialized_latest, rebased_staged) = create_linked_branches(
                    repository.clone(),
                    token,
                    state_current,
                    state_staged,
                    branch,
                    current_branch,
                    head,
                    name.into(),
                    category.into(),
                )
                .await?;

                is_commit = serialized_latest != head;
                head = serialized_latest;
                staged = rebased_staged;
            }

            lore_debug!("Created linked branches, new latest {head}");
        }
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::CreateUnrestored).await;

        create_record(
            repository.clone(),
            branch,
            name,
            category,
            creator,
            created,
            stack,
            head,
            BranchLatestStatus::Divergent,
            name_expected,
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

    Ok(CreatedBranch {
        branch,
        head,
        staged,
    })
}

/// Fails with [`InvalidArguments`] where `branch`, the id of the new branch `name`, is zero.
fn check_new_branch_id(branch: BranchId, name: &str) -> Result<(), BranchError> {
    if branch.is_zero() {
        return Err(BranchError::from(InvalidArguments {
            reason: format!("the id of the new branch {name} is zero"),
        }));
    }
    Ok(())
}

/// The value a create claims `name` from: the value of the name's mapping in the local store, zero
/// for none. A branch the name names with metadata fails with [`BranchAlreadyExists`], unless it is
/// `adoptable`, the branch a dry-run restore reads as freeing the name; zero for none. One without
/// metadata is a stale mapping, which the claim swaps from.
///
/// The mapping is read once, and a read that fails other than as not found fails. Where it holds
/// none, the name is looked up at the remote, then as the derived id (`load_name_to_id_unmapped`).
/// A lookup that finds no branch answers zero, and a refusal fails. A lookup that finds a branch
/// caches it, which may change the mapping, so the mapping is then read again
/// (`unclaimed_name_mapping`). The claim's compare-and-swap refuses a mapping made after the value
/// is read.
async fn name_claim_expected(
    repository: Arc<RepositoryContext>,
    name: &str,
    adoptable: BranchId,
) -> Result<Context, BranchError> {
    let mapped = local_name_mapping(repository.clone(), name).await?;
    let found = if mapped.is_zero() {
        match load_name_to_id_unmapped(repository.clone(), name).await {
            Ok(found) => found,
            Err(err) if err.is_branch_not_found() => return Ok(Context::default()),
            Err(err) => return Err(err),
        }
    } else {
        mapped
    };
    if found == adoptable {
        return Ok(mapped);
    }
    if has_value(repository.clone(), BranchValue::Metadata, found).await? {
        lore_debug!("Branch name {name} already exists with ID {found}");
        return Err(BranchError::from(BranchAlreadyExists {
            branch: name.to_string(),
        }));
    }
    lore_debug!("Stale name mapping for {name} -> {found}, metadata missing");
    if mapped.is_zero() {
        unclaimed_name_mapping(repository, name, found).await
    } else {
        Ok(mapped)
    }
}

/// The value the mapping of `name` holds in the local store, zero for none: the value a claim of
/// the name swaps from. A mapping that names a branch with metadata, as one another writer
/// claimed after the caller's lookup does, fails with [`BranchAlreadyExists`]. A mapping to
/// `unchecked` isn't checked: an id the caller found to have no metadata, or the branch claiming
/// the name. A read that fails other than as not found fails.
async fn unclaimed_name_mapping(
    repository: Arc<RepositoryContext>,
    name: &str,
    unchecked: BranchId,
) -> Result<Context, BranchError> {
    let held = local_name_mapping(repository.clone(), name).await?;
    if !held.is_zero()
        && held != unchecked
        && has_value(repository, BranchValue::Metadata, held).await?
    {
        lore_debug!("Branch name {name} maps to {held}, claimed after its lookup");
        return Err(BranchError::from(BranchAlreadyExists {
            branch: name.to_string(),
        }));
    }
    Ok(held)
}

/// Whether `branch` has `value`, as [`load_local_or_remote`] reads it. A read that fails other
/// than with [`BranchNotFound`] fails.
async fn has_value(
    repository: Arc<RepositoryContext>,
    value: BranchValue,
    branch: BranchId,
) -> Result<bool, BranchError> {
    match load_local_or_remote(repository, value, branch).await {
        Ok(_) => Ok(true),
        Err(err) if err.is_branch_not_found() => Ok(false),
        Err(err) => Err(err).forward::<BranchError>("reading a value of a branch"),
    }
}

/// Restores `branch` under `name` where it has metadata, answering whether it has. Metadata that
/// reads as not found answers `false`, and any other failure to read it fails the restore.
///
/// The restore reads the metadata and makes its checks ([`check_restore`]), then writes in three
/// steps, so that one that stops part way leaves a record of what it held. It swaps the metadata
/// from the value it read to one that names `name` as [`RESTORING`], with [`ARCHIVED`] set where
/// the metadata reads as live, then claims `name` from the value its mapping held
/// ([`name_claim_expected`]), then finishes ([`finish_restore`]). Of restores of one branch that
/// race, one makes the first swap, and the others, which find the metadata changed and a restore
/// recorded, fail with [`BranchAlreadyExists`] naming the branch before they claim anything. A
/// metadata change of another kind sends the restore back to its read.
///
/// A claim that finds the mapping holding another branch, or a value it didn't read, fails with
/// [`BranchAlreadyExists`] naming `name`, and the first swap is set back. One that finds the
/// mapping naming the branch keeps it: once a restore has recorded itself no other can finish, so
/// that mapping is a claim a settled restore made after its settle, and the recorded restore takes
/// it as its own. A branch without a latest revision is restored by the claim alone. A swap or
/// claim that fails other than as described may have landed, and is left for a later restore or
/// archive to settle. A dry run makes the checks alone, and stores nothing.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn, clippy::too_many_arguments)]
fn restore_existing_branch(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    category: &str,
    creator: &str,
    created: u64,
    dry_run: bool,
) -> impl Future<Output = Result<bool, BranchError>> {
    async move {
        let mut yields = false;
        loop {
            let plan =
                match check_restore(repository.clone(), branch, name, dry_run, yields).await? {
                    RestoreCheck::Absent => return Ok(false),
                    RestoreCheck::Settled => {
                        yields = true;
                        continue;
                    }
                    RestoreCheck::Planned(plan) => plan,
                };
            if dry_run {
                return Ok(true);
            }
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreChecked).await;
            let RestorePlan {
                read,
                read_locally,
                mut metadata,
                name_expected,
                latest,
                renamed,
            } = plan;
            if !latest {
                let held = swap_claim(repository, name, name_expected, branch)?
                    .await?
                    .to_context();
                return claimed_name(name, name_expected, held).map(|()| true);
            }

            let expected =
                local_metadata_expected(repository.clone(), branch, read, read_locally).await;
            metadata.remove_key(RELEASED);
            metadata
                .set_string(RESTORING, name)
                .forward::<BranchError>("Failed to record the restore in branch metadata")?;
            if !is_archived(&metadata) {
                stamp_time(&mut metadata, ARCHIVED)?;
                metadata.remove_key(RESTORED);
            }
            let archived = metadata.get_u64(ARCHIVED).ok();
            let recorded =
                match swap_metadata(repository.clone(), branch, expected, &metadata).await {
                    Ok(recorded) => recorded,
                    Err(err) if err.is_branch_already_exists() => {
                        yields = true;
                        continue;
                    }
                    Err(err) => return Err(err),
                };
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreRecorded).await;

            let held = swap_claim(repository.clone(), name, name_expected, branch)?
                .await?
                .to_context();
            if held != branch
                && let Err(err) = claimed_name(name, name_expected, held)
            {
                revert_metadata(repository, branch, recorded, expected).await;
                return Err(err);
            }
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreNameClaimed).await;

            return finish_restore(
                repository, branch, name, category, creator, created, recorded, metadata, renamed,
                archived,
            )
            .await
            .map(|()| true);
        }
    }
}

/// What [`check_restore`] found of the metadata of a branch to restore.
enum RestoreCheck {
    /// The branch has no metadata.
    Absent,
    /// A pending restore was settled, and the metadata is to be read again.
    Settled,
    /// The restore goes on as the plan says.
    Planned(RestorePlan),
}

/// What a restore that passed [`check_restore`] goes on with.
struct RestorePlan {
    /// The metadata hash the check read.
    read: Hash,
    /// Whether `read` came from the local store.
    read_locally: bool,
    /// The metadata `read` names, without the pending restore a dry run reads as settled.
    metadata: Metadata,
    /// The value the claim swaps the name's mapping from ([`name_claim_expected`]).
    name_expected: Context,
    /// Whether the branch has a latest revision, or its metadata records an archive or a restore.
    latest: bool,
    /// Whether the metadata names the branch other than the name it is restored under.
    renamed: bool,
}

/// Reads the metadata of `branch` for a restore under `name`, and makes the restore's checks.
///
/// Metadata that holds a [`RESTORING`] entry belongs to a restore in progress, or one that
/// stopped: the restore takes it over ([`settle_pending_restore`]) and reads again. One that
/// `yields`, as a restore that lost its first swap or settled a pending restore does, fails with
/// [`BranchAlreadyExists`] naming the branch instead: that restore started since. Metadata that
/// names a [`RELEASED`] name, the claim of a settled restore, which may still map to the branch,
/// has the name released first ([`unmarked`]), and the first swap removes the entry, so
/// a restore that stopped part way leaves no name a later restore of the branch, under any name,
/// can't free, while the entry stays.
///
/// The metadata says whether the branch is archived ([`is_archived`]). One it says is live is,
/// where its name maps to it, as [`load_name_to_id`] looks it up, and the check fails with
/// [`BranchAlreadyExists`] naming it; otherwise an older version archived it, which removed the
/// name and kept [`RESTORED`]. Where the metadata names the branch other than `name`, or not at
/// all, the restore renames it: the old name must not map to it, as that lookup, made once, finds,
/// or the check fails with [`BranchAlreadyExists`] naming the old name. A lookup that fails other
/// than as not found fails.
///
/// Metadata without [`ARCHIVED`] or [`RESTORED`] predates them, or belongs to a create that
/// hasn't finished. Such a branch without a latest revision, as [`load_local_or_remote`] reads it,
/// is one whose create hasn't stored it yet, as a create stores it only once it has claimed its
/// name ([`create_record`]), or one made from an empty default branch and archived by an older
/// version: it is restored only under the name its metadata holds, and a rename fails with
/// [`BranchAlreadyExists`] naming the branch. The latest revision is read before the lookup of the
/// old name, so a create that stores it between the two has claimed its name before the lookup.
///
/// A dry run reads a pending restore as settled and a [`RELEASED`] name as released, so that name
/// may map to the branch, as the claim of `name` or as the old name.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn check_restore(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    dry_run: bool,
    yields: bool,
) -> impl Future<Output = Result<RestoreCheck, BranchError>> {
    async move {
        let (read, read_locally) =
            match load_local_or_remote(repository.clone(), BranchValue::Metadata, branch).await {
                Ok(read) => read,
                Err(err) if err.is_branch_not_found() => return Ok(RestoreCheck::Absent),
                Err(err) => return Err(err),
            };
        let mut metadata = load_metadata(repository.clone(), read).await?;
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::RestoreMetadataRead).await;
        let mut freed = None;
        if let Some(pending) = restoring(&metadata) {
            if yields {
                lore_debug!("A restore of {branch} under {pending} recorded itself first");
                return Err(BranchError::from(BranchAlreadyExists {
                    branch: branch.to_string(),
                }));
            }
            if !dry_run {
                let pending = pending.to_string();
                settle_pending_restore(
                    repository.clone(),
                    branch,
                    read,
                    read_locally,
                    metadata,
                    &pending,
                )
                .await?;
                return Ok(RestoreCheck::Settled);
            }
            freed = Some(pending.to_string());
            metadata.remove_key(RESTORING);
        }
        if let Some(marked) = released(&metadata) {
            if dry_run {
                freed = Some(marked.to_string());
            } else {
                release_unless(repository.clone(), branch, marked, unmarked).await?;
            }
        }
        let freed_name = |held: &str| freed.as_deref().is_some_and(|freed| fold_eq(freed, held));
        let adoptable = if freed_name(name) {
            branch
        } else {
            BranchId::default()
        };
        let existing_name = branch::name(&metadata).unwrap_or("").to_string();
        let stamped = metadata.get_u64(ARCHIVED).is_ok() || metadata.get_u64(RESTORED).is_ok();
        let live = stamped && !is_archived(&metadata);
        if live {
            refuse_live_old_name(repository.clone(), branch, &existing_name).await?;
            lore_debug!("Branch {branch} reads as live, but an older version archived it");
        }
        let name_expected = name_claim_expected(repository.clone(), name, adoptable).await?;
        let latest = stamped || has_value(repository.clone(), BranchValue::Latest, branch).await?;

        let renamed = existing_name != name;
        match existing_name.as_str() {
            _ if renamed && !latest => {
                lore_debug!("Branch {branch} has no latest revision, so it isn't renamed {name}");
                return Err(BranchError::from(BranchAlreadyExists {
                    branch: branch.to_string(),
                }));
            }
            "" => lore_info!("Restoring branch with partial metadata as '{name}' ({branch})"),
            old_name if renamed => {
                if !live && !freed_name(old_name) {
                    refuse_live_old_name(repository.clone(), branch, old_name).await?;
                }
                lore_info!("Restoring deleted branch '{old_name}' as '{name}' ({branch})");
            }
            _ => lore_info!("Restoring deleted branch '{name}' ({branch})"),
        }
        Ok(RestoreCheck::Planned(RestorePlan {
            read,
            read_locally,
            metadata,
            name_expected,
            latest,
            renamed,
        }))
    }
}

/// Finishes the restore of `branch` under `name` that recorded itself as `recorded`, holding
/// `metadata` with [`ARCHIVED`] at `archived`, and claimed the name: a swap from the record to
/// `metadata` without [`RESTORING`], named `name` where `renamed`, with missing descriptive fields
/// filled in from `category`, `creator` and `created`, and [`RESTORED`] set to the time now, which
/// says the branch is live.
///
/// A final swap that finds the metadata changed reads it again. Where it still holds the record,
/// with the [`ARCHIVED`] time the first swap left, a protect or a metadata edit changed it, and the
/// final swap is made again from it. Otherwise a takeover settled the record, as it moves that
/// time on, and the restore fails with [`BranchAlreadyExists`] naming the branch; the claim is
/// released where the metadata then neither records a restore under `name` nor names the branch
/// live under it, as one made after a takeover released the name is. A read that fails fails the
/// restore and leaves the record for a later restore or archive to settle. A final swap that fails
/// otherwise, and left the metadata holding its value, has finished the restore.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn, clippy::too_many_arguments)]
fn finish_restore(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    category: &str,
    creator: &str,
    created: u64,
    mut recorded: Hash,
    mut metadata: Metadata,
    renamed: bool,
    archived: Option<u64>,
) -> impl Future<Output = Result<(), BranchError>> {
    async move {
        loop {
            metadata.remove_key(RESTORING);
            if renamed {
                metadata
                    .set_string(NAME, name)
                    .forward::<BranchError>("Failed to update branch name metadata")?;
            }
            patch_missing_metadata_fields(&mut metadata, category, creator, created)?;
            stamp_time(&mut metadata, RESTORED)?;
            let finished = serialize_metadata(repository.clone(), &metadata).await?;
            match swap_metadata_key(repository.clone(), branch, recorded, finished).await {
                Ok(()) => return Ok(()),
                Err(err) if err.is_branch_already_exists() => {
                    let Some((current, current_metadata)) =
                        restore_record(repository.clone(), branch, name, archived).await?
                    else {
                        release_unrecorded_claim(repository, branch, name).await;
                        return Err(err);
                    };
                    lore_debug!("Branch {branch} changed since its restore recorded itself");
                    recorded = current;
                    metadata = current_metadata;
                }
                Err(err) => {
                    return match mutable_load(repository, METADATA, branch).await {
                        Ok(held) if held == finished => Ok(()),
                        _ => Err(err),
                    };
                }
            }
        }
    }
}

/// The local metadata of `branch` and its hash, where it still holds the record of the restore
/// under `name` whose first swap left [`ARCHIVED`] at `archived`: a settle moves that time on, so
/// metadata that holds both was changed by a protect or a metadata edit alone. `None` where it
/// holds another record or none. A read that fails fails.
async fn restore_record(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    archived: Option<u64>,
) -> Result<Option<(Hash, Metadata)>, BranchError> {
    let current = mutable_load(repository.clone(), METADATA, branch).await?;
    let metadata = load_metadata(repository, current).await?;
    let held = restoring(&metadata) == Some(name) && metadata.get_u64(ARCHIVED).ok() == archived;
    Ok(held.then_some((current, metadata)))
}

/// The name a restore of the branch `metadata` describes has recorded as the one it claims, as
/// [`RESTORING`] holds it: a restore in progress, or one that stopped.
fn restoring(metadata: &Metadata) -> Option<&str> {
    metadata.get_string(RESTORING).ok()
}

/// The name a settled restore of the branch `metadata` describes had claimed, as [`RELEASED`]
/// holds it, which may still map to the branch.
fn released(metadata: &Metadata) -> Option<&str> {
    metadata.get_string(RELEASED).ok()
}

/// Settles the restore recorded in `metadata`, the metadata of `branch` a read found as `read`,
/// which claims `pending`: it swaps the metadata from `read` to one that names `pending` as
/// [`RELEASED`] in place of the record, with [`ARCHIVED`] set past the time it holds, as an archive
/// does, which fails the restore's final swap, then releases `pending` from the branch while the
/// metadata still marks it ([`unmarked`]). A metadata key that holds another value is
/// left as it is, and answers `None`; a settled restore answers the hash its swap wrote, and
/// whether its release took `pending` from the branch. A write that fails otherwise fails.
///
/// The swap comes first, so the restore can't finish once its name is released, and it moves the
/// archive time on, so no later record of a restore makes the metadata the settled restore swaps
/// from again. The [`RELEASED`] entry outlives the settle, so a settle that stops before its
/// release, or a claim the restore makes after it and then stops, leaves the name for the next
/// restore or archive to release. A restore that claims after the release and goes on finds its
/// final swap refused, and releases its claim itself.
async fn settle_pending_restore(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    read: Hash,
    read_locally: bool,
    mut metadata: Metadata,
    pending: &str,
) -> Result<Option<(Hash, bool)>, BranchError> {
    metadata.remove_key(RESTORING);
    metadata
        .set_string(RELEASED, pending)
        .forward::<BranchError>("Failed to record the settled restore in branch metadata")?;
    stamp_time(&mut metadata, ARCHIVED)?;
    let expected = local_metadata_expected(repository.clone(), branch, read, read_locally).await;
    let settled = match swap_metadata(repository.clone(), branch, expected, &metadata).await {
        Ok(settled) => settled,
        Err(err) if err.is_branch_already_exists() => return Ok(None),
        Err(err) => return Err(err),
    };
    lore_info!("Settled a restore of branch {branch} under '{pending}' that didn't finish");
    #[cfg(feature = "test-util")]
    crate::test_util::branch_step(crate::test_util::BranchStep::PendingRestoreSettled).await;
    let released = release_unless(repository, branch, pending, unmarked).await?;
    Ok(Some((settled, released)))
}

/// Caches `remote`, the remote's metadata hash of `branch`, in the local metadata key.
///
/// Where the local store holds metadata, the cache first settles a restore it records
/// ([`RESTORING`]), or releases the name it marks [`RELEASED`], as a restore or an archive does.
/// Otherwise nothing would free the name a stopped local restore claimed once the remote's metadata
/// replaced the record. The cache then swaps the key from the value it left. A local write made
/// after that, such as a settle's, stays, and the cache fails with [`BranchAlreadyExists`] naming
/// the branch. Local metadata whose blob can't be read is replaced. A write that fails fails.
///
/// The settle runs on a cache miss, and is boxed: inline, it would make every future awaiting a
/// name lookup as large as its own, whether or not the lookup caches anything.
pub(crate) async fn cache_remote_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    remote: Hash,
) -> Result<(), BranchError> {
    let local = match mutable_load(repository.clone(), METADATA, branch).await {
        Ok(read) => Box::pin(settle_local_restore(repository.clone(), branch, read)).await?,
        Err(err) if err.is_branch_not_found() => Hash::default(),
        Err(err) => return Err(err),
    };
    swap_metadata_key(repository, branch, local, remote).await
}

/// Settles a restore the local metadata of `branch`, which the local key holds as `read`, records,
/// or releases the name it marks released, as [`cache_remote_metadata`] describes, answering the
/// value the local metadata key then holds.
async fn settle_local_restore(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    mut read: Hash,
) -> Result<Hash, BranchError> {
    loop {
        let Ok(metadata) = load_metadata(repository.clone(), read).await else {
            return Ok(read);
        };
        let Some(pending) = restoring(&metadata).map(str::to_string) else {
            if let Some(marked) = released(&metadata) {
                release_unless(repository, branch, marked, unmarked).await?;
            }
            return Ok(read);
        };
        if let Some((settled, _released)) =
            settle_pending_restore(repository.clone(), branch, read, true, metadata, &pending)
                .await?
        {
            return Ok(settled);
        }
        read = match mutable_load(repository.clone(), METADATA, branch).await {
            Ok(read) => read,
            Err(err) if err.is_branch_not_found() => return Ok(Hash::default()),
            Err(err) => return Err(err),
        };
    }
}

/// Releases `name` from `branch`, the claim of a restore whose final swap found the metadata
/// changed, unless the metadata holds it ([`release_unless`], [`holds_name`]), logging a failure.
/// A takeover releases the name itself where the restore had claimed it when the takeover read the
/// metadata; this frees one claimed after.
async fn release_unrecorded_claim(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
) {
    if let Err(err) = release_unless(repository, branch, name, holds_name).await {
        lore_warn!(
            "Failed to release the name {name} of branch {branch}, whose restore didn't finish: {err}"
        );
    }
}

/// Releases `name` from `branch` unless the branch's metadata, read again first with
/// [`metadata_local`], `keeps` it, and answers whether it released it. A mapping that names
/// another branch, or none, is left as it is, and answers false. A read or release that fails
/// otherwise fails.
async fn release_unless(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    keeps: fn(&Metadata, &str) -> bool,
) -> Result<bool, BranchError> {
    if keeps(&metadata_local(repository.clone(), branch).await?, name) {
        return Ok(false);
    }
    release_mapped_name(repository, branch, name).await
}

/// Whether `metadata` records a restore under `name` or names its branch live under it, compared
/// as [`restores_name`] and [`names_branch`] compare: the mapping of `name` to the branch then
/// belongs to that restore.
fn holds_name(metadata: &Metadata, name: &str) -> bool {
    restores_name(metadata, name) || (!is_archived(metadata) && names_branch(metadata, name))
}

/// Whether `metadata` names its branch `name`, or records a restore under it, archived or not, as
/// a create or a restore that holds the name's mapping leaves it. A mapping of `name` to the
/// branch it doesn't account for is a claim that no create or restore of the branch holds. A
/// restore leaves one when it claims its name after its settle and after a later restore removed
/// the [`RELEASED`] mark, and then stops.
fn accounts_for_name(metadata: &Metadata, name: &str) -> bool {
    restores_name(metadata, name) || names_branch(metadata, name)
}

/// Whether `metadata` records a restore under `name` ([`RESTORING`]), compared regardless of case,
/// as the name's mapping key is ([`fold_eq`]).
fn restores_name(metadata: &Metadata, name: &str) -> bool {
    restoring(metadata).is_some_and(|held| fold_eq(held, name))
}

/// Whether `metadata` names its branch `name`, compared regardless of case, as the name's mapping
/// key is ([`fold_eq`]).
fn names_branch(metadata: &Metadata, name: &str) -> bool {
    branch::name(metadata).is_ok_and(|held| fold_eq(held, name))
}

/// Whether `metadata` no longer marks `name` [`RELEASED`], so a release of a marked name keeps it
/// ([`release_unless`]). A restore removes the mark after it releases the name, and
/// so does a lookup that caches the remote's metadata over the local one
/// ([`cache_remote_metadata`]). Metadata without the mark may since name the branch under `name`,
/// live or archived.
fn unmarked(metadata: &Metadata, name: &str) -> bool {
    released(metadata) != Some(name)
}

/// Releases `name` from `branch` with [`release_name`], answering whether it did: a mapping that
/// names another branch, or none, is no failure, and answers false.
async fn release_mapped_name(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
) -> Result<bool, BranchError> {
    match release_name(repository, name, branch).await {
        Ok(()) => Ok(true),
        Err(err) if err.is_branch_not_found() => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether `metadata` says its branch is archived: it holds an [`ARCHIVED`] time and no
/// [`RESTORED`] time, as an archive leaves it. Metadata with neither predates them, or belongs to
/// a create that hasn't finished, and says nothing.
fn is_archived(metadata: &Metadata) -> bool {
    metadata.get_u64(ARCHIVED).is_ok() && metadata.get_u64(RESTORED).is_err()
}

/// Fails with [`BranchAlreadyExists`] naming `old_name` where it maps to `branch`, as
/// [`load_name_to_id`] looks it up: the branch is live under it, so a restore can't rename it. A
/// lookup that fails other than as not found fails.
async fn refuse_live_old_name(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    old_name: &str,
) -> Result<(), BranchError> {
    match load_name_to_id(repository, old_name).await {
        Ok(held) if held == branch => {
            lore_debug!("Branch ID {branch} already exists under name '{old_name}'");
            Err(BranchError::from(BranchAlreadyExists {
                branch: old_name.to_string(),
            }))
        }
        Ok(_) => Ok(()),
        Err(err) if err.is_branch_not_found() => Ok(()),
        Err(err) => Err(err),
    }
}

/// Fails with [`BranchAlreadyExists`] naming `name` unless `held`, the id a claim of `name` from
/// `expected` found, is `expected`: only then did the claim write the mapping.
fn claimed_name(name: &str, expected: Context, held: Context) -> Result<(), BranchError> {
    if held == expected {
        return Ok(());
    }
    lore_debug!("Name {name} maps to {held}, not {expected}, so the restore doesn't hold it");
    Err(BranchError::from(BranchAlreadyExists {
        branch: name.to_string(),
    }))
}

/// Sets the time `key` holds in `metadata` to the time now in milliseconds, or one past the time
/// it holds where that is later, so a swap of the metadata key to the result writes a value other
/// than the one it replaces.
fn stamp_time(metadata: &mut Metadata, key: &str) -> Result<(), BranchError> {
    let after = metadata
        .get_u64(key)
        .map_or(0, |time| time.saturating_add(1));
    metadata
        .set_u64(key, crate::util::time::timestamp().max(after))
        .forward::<BranchError>("Failed to set a time in branch metadata")
}

/// The value a swap of the metadata key of `branch` from `read`, the hash a read of the metadata
/// found, expects the local key to hold: `read` where the read was local, or where a lookup has
/// since cached the remote's value locally, and zero otherwise, as the local key held none. A key
/// holding another value then fails the swap.
async fn local_metadata_expected(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    read: Hash,
    read_locally: bool,
) -> Hash {
    if read_locally {
        return read;
    }
    match mutable_load(repository, METADATA, branch).await {
        Ok(local) if local == read => read,
        _ => Hash::default(),
    }
}

/// Serializes `metadata` and swaps it into the metadata key of `branch` from `expected`,
/// answering its hash, as [`swap_metadata_key`] swaps it.
async fn swap_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    expected: Hash,
    metadata: &Metadata,
) -> Result<Hash, BranchError> {
    let hash = serialize_metadata(repository.clone(), metadata).await?;
    swap_metadata_key(repository, branch, expected, hash).await?;
    Ok(hash)
}

/// Stores `metadata` in the immutable store, answering its hash.
async fn serialize_metadata(
    repository: Arc<RepositoryContext>,
    metadata: &Metadata,
) -> Result<Hash, BranchError> {
    metadata
        .serialize(repository)
        .await
        .forward::<BranchError>("Failed to serialize branch metadata")
}

/// Swaps the metadata key of `branch` from `expected` to `hash`. A key that holds another value is
/// left as it is, and the swap fails with [`BranchAlreadyExists`] naming the branch.
async fn swap_metadata_key(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    expected: Hash,
    hash: Hash,
) -> Result<(), BranchError> {
    let held = mutable_try_store(repository, METADATA, branch, expected, hash).await?;
    if held != expected {
        lore_debug!("Branch {branch} has metadata {held}, not {expected}, so it isn't rewritten");
        return Err(BranchError::from(BranchAlreadyExists {
            branch: branch.to_string(),
        }));
    }
    Ok(())
}

/// Writes the record of a new branch: its metadata, swapped into the metadata key of `branch` from
/// zero, then the mapping of `name` to `branch`, claimed from `name_expected`, then `latest` as its
/// latest revision with `status`. Sends no event. The latest revision comes last, once the name
/// is claimed: a restore that renames a branch refuses one without a latest revision, so it can't
/// rename a branch whose create is between its metadata and its claim (`check_restore`).
///
/// A metadata key that holds any value but zero, as another create of `branch` leaves it, is left
/// as it is, and the write fails with [`BranchAlreadyExists`] naming `branch`, claiming nothing
/// and writing no latest revision.
///
/// `name_expected` is the value the caller read from the mapping: zero for none, or the id of a
/// stale mapping, whose branch has no metadata. A failed claim swaps the metadata
/// key back to zero, which removes it, writes no latest revision, and fails with the
/// claim's error: [`BranchAlreadyExists`] where the mapping holds another value. A claim that
/// landed before its error is left as a stale mapping, to a branch without metadata. A zero
/// `branch` fails with [`InvalidArguments`] and writes nothing.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn, clippy::too_many_arguments)]
pub fn create_record(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    category: &str,
    creator: &str,
    created: u64,
    stack: Vec<BranchPoint>,
    latest: Hash,
    status: BranchLatestStatus,
    name_expected: Context,
) -> impl Future<Output = Result<(), BranchError>> {
    async move {
        check_new_branch_id(branch, name)?;
        let metadata_hash = {
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
            metadata
                .serialize(repository.clone())
                .await
                .forward::<BranchError>("Failed to serialize branch metadata")?
        };
        let held = mutable_try_store(
            repository.clone(),
            METADATA,
            branch,
            Hash::default(),
            metadata_hash,
        )
        .await?;
        if !held.is_zero() {
            lore_debug!("Branch {branch} has metadata {held}, so {name} is not claimed for it");
            return Err(BranchError::from(BranchAlreadyExists {
                branch: branch.to_string(),
            }));
        }
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::CreateMetadataStored).await;

        if let Err(err) = claim_name(repository.clone(), name, name_expected, branch).await {
            revert_metadata(repository, branch, metadata_hash, Hash::default()).await;
            return Err(err);
        }
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::CreateNameClaimed).await;

        store_latest(repository, branch, Hash::default(), latest, status).await
    }
}

/// Swaps the metadata of `branch` from `stored` back to `previous`, after a claim that failed. The
/// key keeps any value but `stored`, so a value another writer stored since stays unless it is
/// `stored` too. A failed write is logged.
async fn revert_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    stored: Hash,
    previous: Hash,
) {
    if let Err(err) = mutable_try_store(repository, METADATA, branch, stored, previous).await {
        lore_warn!(
            "Failed to set the metadata of branch {branch}, whose claim of its name failed, back to {previous}: {err}"
        );
    }
}

/// A link row the cascade repins, held by the staged state or the current one.
#[derive(Clone, Copy)]
struct CascadeMount {
    link: state::LinkReference,
    staged: bool,
}

/// The auto-following links `state_staged` and `state` hold, grouped by linked repository.
///
/// The staged state's rows come first, so a group's first row is the pin the next commit takes.
/// A row of `state` that the staged state removes or remounts stays in its group, so the link is
/// on the new branch once the change is unstaged.
///
/// Grouping keeps the cascade from racing itself: concurrent creates with the same branch ID
/// resolve to one winner, and the loser is rejected with "branch has been advanced by another
/// instance", failing the whole create.
async fn cascade_mounts(
    repository: &Arc<RepositoryContext>,
    state: &State,
    state_staged: Option<&Arc<State>>,
) -> Result<Vec<(RepositoryId, Vec<CascadeMount>)>, BranchError> {
    let staged_links = match state_staged {
        Some(state_staged) => state_staged
            .link_list(repository.clone())
            .await
            .forward::<BranchError>("listing staged links")?,
        None => Vec::new(),
    };
    let current_links = state
        .link_list(repository.clone())
        .await
        .forward::<BranchError>("listing links")?;

    let mounts = staged_links
        .into_iter()
        .map(|link| CascadeMount { link, staged: true })
        .chain(current_links.into_iter().map(|link| CascadeMount {
            link,
            staged: false,
        }))
        .filter(|mount| mount.link.flags & link::LinkFlags::DisableAutoFollow == 0);
    Ok(link::group_by_repository(mounts, |mount| {
        mount.link.repository
    }))
}

/// Creates `branch` in the linked repositories, and moves each link that names an explicit
/// branch onto it.
///
/// The links of `state_staged` and `state` both drive the cascade, the staged ones first, so a
/// linked branch starts at the pin the next commit takes. A row naming an explicit branch is
/// repinned in the state holding it, at its own pin, with the zero branch to follow the new one.
///
/// Repinning `state` writes a revision on top of `current_latest`, which is returned as the
/// branch's latest, and `state_staged` is rebased onto it.
#[allow(clippy::too_many_arguments)]
async fn create_linked_branches(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: Arc<State>,
    state_staged: Option<Arc<State>>,
    branch: BranchId,
    current_branch: BranchId,
    current_latest: Hash,
    name: String,
    category: String,
) -> Result<(Hash, Option<Hash>), BranchError> {
    let link_groups = cascade_mounts(&repository, &state, state_staged.as_ref()).await?;

    if link_groups.is_empty() {
        return Ok((current_latest, None));
    }

    let mut link_tasks = JoinSet::new();

    for (link_id, mounts) in link_groups {
        lore_spawn!(link_tasks, {
            let link = repository.to_link_context(link_id).await;
            let link_remote = link.remote().await.forward_with::<BranchError, _>(|| {
                format!("Failed to connect to link repository {link_id}")
            })?;

            let repository = repository.clone();
            let state = state.clone();
            let state_staged = state_staged.clone();
            let branch_id = branch;
            let branch_name = name.clone();
            let branch_category = category.clone();

            async move {
                // The first mount seeds the branch point; the others adopt what
                // it resolved to, since they all address the same branch.
                let leader = mounts[0];
                let resolved_parent_branch = leader.link.resolve_branch(current_branch);

                let outcome = link::create_branch(
                    link.clone(),
                    link_remote,
                    branch_id,
                    branch_name,
                    branch_category,
                    resolved_parent_branch,
                    leader.link.signature,
                )
                .await
                .forward_with::<BranchError, _>(|| {
                    format!("Failed to create branch for link repository {link_id}")
                })?;

                for mount in mounts.iter() {
                    let mount_state = match &state_staged {
                        Some(state_staged) if mount.staged => state_staged,
                        _ => &state,
                    };

                    // The create is shared, the reporting is not: every mount of the state that
                    // seeded the branch reports its own outcome, keyed on its path, because the
                    // repository ID cannot tell the mounts apart.
                    if mount.staged == leader.staged {
                        let link_path = mount_state
                            .node_path(repository.clone(), mount.link.local_node)
                            .await
                            .unwrap_or_default();

                        link::report_branch_outcome(
                            &link_path,
                            link_id,
                            branch_id,
                            outcome.revision,
                            outcome.reused,
                        );
                    }

                    // A row that tracks its parent's branch already resolves to the new one, at
                    // the same revision, so it is left alone.
                    if !mount.link.branch.is_zero() {
                        link::update_link_pin_by_node(
                            mount_state,
                            repository.clone(),
                            mount.link.repository,
                            BranchId::default(),
                            mount.link.signature,
                            mount.link.local_node,
                        )
                        .await
                        .forward_with::<BranchError, _>(|| {
                            format!(
                                "Failed to update link reference for link repository {}",
                                mount.link.repository
                            )
                        })?;
                    }
                }

                Ok(())
            }
        });
    }

    lore_drain_tasks!(link_tasks, BranchError::internal("Task failed"))?;

    let head = if state.is_dirty() {
        store_repin_revision(&repository, token, &state, branch, current_latest).await?
    } else {
        current_latest
    };

    let staged = match state_staged {
        Some(state_staged) if head != current_latest => {
            state_staged.reparent_replacing(current_latest, head);
            Some(state_staged)
        }
        Some(state_staged) if state_staged.is_dirty() => Some(state_staged),
        _ => None,
    };
    let staged = match staged {
        Some(state_staged) => Some(
            state_staged
                .serialize(repository.clone(), token)
                .await
                .forward::<BranchError>("Failed to serialize staged state")?,
        ),
        None => None,
    };

    Ok((head, staged))
}

/// Writes the repins `state` holds as a revision on top of `current_latest`.
async fn store_repin_revision(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: &Arc<State>,
    branch: BranchId,
    current_latest: Hash,
) -> Result<Hash, BranchError> {
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

    commit::weave_history(
        repository.clone(),
        state.clone(),
        commit::BasedOn::FirstParent,
    )
    .await
    .forward::<BranchError>("Failed to weave history")?;

    state
        .serialize(repository.clone(), token)
        .await
        .forward::<BranchError>("Failed to serialize revision state")
}

/// Archives `branch`, named `name`, whose metadata the metadata key holds locally as `expected`.
/// It swaps the metadata from `expected` to `metadata` with [`ARCHIVED`] set past the time it
/// holds and [`RESTORED`] removed, which says the branch is archived, then releases the name from
/// the branch with a compare-and-swap of the name's mapping ([`release_archived_name`]). A
/// metadata key that holds another value is left as it is, and the archive fails with
/// [`BranchAlreadyExists`] naming the branch, releasing nothing. A mapping that names another
/// branch, or none, is left as it is, and the archive fails with [`BranchNotFound`]. The branch
/// keeps its latest revision. Sends no event.
///
/// The swap comes first, so a restore that read the metadata before it can't finish once the name
/// is released. A restore that reads it after can, where the name no longer maps to the branch,
/// and the release then leaves the name with it. `metadata` must hold no pending restore
/// ([`RESTORING`]): a caller settles one first, as [`delete`] does. A [`RELEASED`] entry stays.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn archive_record(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    expected: Hash,
    metadata: Metadata,
) -> impl Future<Output = Result<(), BranchError>> {
    async move {
        let archived = {
            let mut metadata = metadata;
            stamp_time(&mut metadata, ARCHIVED)?;
            metadata.remove_key(RESTORED);
            swap_metadata(repository.clone(), branch, expected, &metadata).await?
        };
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::ArchiveReleasing).await;
        release_archived_name(repository, branch, name, archived).await
    }
}

/// Releases `name` from `branch` with [`release_name`], once an archive has swapped the branch's
/// metadata to `archived`, unless a restore holds the name since: the metadata key is read again,
/// and where it holds another value, the metadata it holds records a restore under `name` or names
/// the branch live under it ([`holds_name`]). A restore that read the archive's metadata claims
/// the name only where it no longer maps to the branch, as one a failed restore released after the
/// archive's lookup. A key that still holds `archived` is released from without a metadata read.
/// A read that fails fails.
async fn release_archived_name(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    archived: Hash,
) -> Result<(), BranchError> {
    let current = mutable_load(repository.clone(), METADATA, branch).await?;
    if current != archived && holds_name(&load_metadata(repository.clone(), current).await?, name) {
        lore_debug!("A restore of {branch} holds {name} since its archive");
        return Ok(());
    }
    release_name(repository, name, branch).await
}

/// Archives `branch` with [`archive_record`], and sends a `BranchArchive` event where `repository`
/// is neither a layer nor a link. Refuses the current branch, a protected branch, the default
/// branch and a branch without a branch point. The name in the branch's metadata must map to the
/// branch, as [`load_name_to_id`] looks it up, and otherwise the archive fails with
/// [`BranchNotFound`]. A remote that refuses the lookup fails the archive with its refusal. A
/// branch archived already fails the same way, unless the local store maps its name to nothing and
/// the remote's mapping, or the id derived from the name, names the branch: the lookup caches that
/// mapping and the remote's metadata, and the branch is archived again. Metadata that says the
/// branch is archived while its name maps to it, as an older version's restore or an archive that
/// stopped between its swap and its release leaves it, is archived the same way.
///
/// Metadata that records a restore in progress, or one that stopped ([`RESTORING`]), is settled
/// ([`settle_pending_restore`]): the archive swaps the record out and releases the name it names,
/// then reads again. A branch whose name then maps to it, as one an older version restored while
/// the record stayed, is archived as above. A metadata key that changed since the archive read it,
/// as a restore, a protect or a metadata edit leaves it, sends the archive back to its read. A
/// [`RELEASED`] name other than the branch's own, which a settled restore had claimed, is released
/// before the lookup ([`unmarked`]), and the entry stays for the next restore to release
/// again and remove. A branch whose name doesn't map to it then fails the archive with
/// [`BranchNotFound`], as the branch is archived, unless this archive's settle released a name
/// that mapped to the branch, the claim of the restore it settled: the branch read as live under
/// that name, and the archive succeeds. So of two archives that settle one restore, one succeeds,
/// and an archive that settles a restore stopped before its claim fails, as does one that
/// releases only a [`RELEASED`] name, the claim of a restore settled before.
///
/// A client's dry run makes the checks and sends the event, but archives nothing. That relies on
/// `repository` being built by `RepositoryContext::new_with_state`, or derived from a context it
/// built, as every context of the CLI's archive is: its store then discards the mapping the lookup
/// caches for a name only the remote or the derived id maps, so a release would find no mapping
/// and fail.
pub async fn delete(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<(), BranchError> {
    // Do not allow deleting the current branch
    if let Ok((_revision, current_branch)) = crate::instance::load_current_anchor(&repository).await
        && current_branch == branch
    {
        return Err(BranchError::from(DeleteCurrent {
            branch: branch.to_string(),
        }));
    }

    let discards = repository::discards_mutable_writes();
    let mut settled = false;
    let mut released_name = false;
    let branch_name = loop {
        let (read, read_locally, branch_metadata) =
            archivable_metadata(&repository, branch).await?;
        let branch_name = name(&branch_metadata).unwrap_or_default().to_string();
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::ArchiveMetadataRead).await;
        if let Some(pending) = restoring(&branch_metadata) {
            if discards {
                break branch_name;
            }
            let pending = pending.to_string();
            if let Some((_settled, released)) = settle_pending_restore(
                repository.clone(),
                branch,
                read,
                read_locally,
                branch_metadata,
                &pending,
            )
            .await?
            {
                settled = true;
                released_name |= released;
            }
            continue;
        }
        if !settled
            && !discards
            && let Some(marked) = released(&branch_metadata)
            && !fold_eq(marked, &branch_name)
        {
            release_unless(repository.clone(), branch, marked, unmarked).await?;
        }

        let mapped = match load_name_to_id(repository.clone(), &branch_name).await {
            Ok(held) => held == branch,
            Err(err) if err.is_branch_not_found() => false,
            Err(err) => return Err(err),
        };
        if !mapped {
            if released_name {
                break branch_name;
            }
            return Err(BranchError::from(BranchNotFound {
                branch: branch_name,
            }));
        }
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::ArchiveLookedUp).await;
        if discards {
            break branch_name;
        }
        let expected =
            local_metadata_expected(repository.clone(), branch, read, read_locally).await;
        match archive_record(
            repository.clone(),
            branch,
            &branch_name,
            expected,
            branch_metadata,
        )
        .await
        {
            Ok(()) => break branch_name,
            Err(err) if err.is_branch_already_exists() => {
                lore_debug!("Branch {branch} changed since the archive read it as {branch_name}");
            }
            Err(err) => return Err(err),
        }
    };

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

/// The metadata of `branch`, as [`load_local_or_remote`] reads it, with its hash and whether the
/// read was local, where [`delete`] may archive the branch. A protected branch, the default branch
/// and one without a branch point are refused. A refusal of the read passes on; metadata that can't
/// be read otherwise, or holds no name, fails with [`BranchNotFound`] naming the branch.
async fn archivable_metadata(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<(Hash, bool, Metadata), BranchError> {
    let unreadable = |err: BranchError| {
        if is_refusal(&err) {
            return err;
        }
        BranchError::BranchNotFound(
            BranchNotFound {
                branch: branch.to_string(),
            }
            .chain_err_from(err, "reading the metadata of the branch to archive"),
        )
    };
    let (read, read_locally) =
        load_local_or_remote(repository.clone(), BranchValue::Metadata, branch)
            .await
            .map_err(unreadable)?;
    let branch_metadata = load_metadata(repository.clone(), read)
        .await
        .map_err(unreadable)?;
    let branch_name = name(&branch_metadata).unwrap_or_default();
    let display = if branch_name.is_empty() {
        branch.to_string()
    } else {
        branch_name.to_string()
    };

    if branch_metadata.get_bool(PROTECT).unwrap_or_default() {
        return Err(BranchError::from(DeleteProtected { branch: display }));
    }
    if let Ok(repository_metadata) = repository::metadata_hash(repository.clone()).await
        && let Ok(repository_metadata) =
            repository::metadata(repository.clone(), repository_metadata).await
        && repository_metadata.default_branch == branch
    {
        return Err(BranchError::from(DeleteDefault { branch: display }));
    }
    if stack(&branch_metadata).is_empty() {
        return Err(BranchError::from(DeleteDefault { branch: display }));
    }

    if branch_name.is_empty() {
        return Err(BranchError::from(BranchNotFound {
            branch: branch.to_string(),
        }));
    }
    Ok((read, read_locally, branch_metadata))
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
        // A swap from the value read, so a protect never puts back metadata an archive or a
        // restore swapped since; one that finds it changed reads again.
        loop {
            let (read, read_locally) =
                load_local_or_remote(repository.clone(), BranchValue::Metadata, branch).await?;
            let mut branch_metadata = load_metadata(repository.clone(), read).await?;
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::ProtectMetadataRead).await;
            branch_metadata
                .set_bool(PROTECT, value)
                .forward::<BranchError>("Failed to update branch protect metadata")?;
            let expected =
                local_metadata_expected(repository.clone(), branch, read, read_locally).await;
            match swap_metadata(repository.clone(), branch, expected, &branch_metadata).await {
                Ok(_) => break,
                Err(err) if err.is_branch_already_exists() => {}
                Err(err) => return Err(err),
            }
        }
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

/// Removes the branches the mappings of `repository` name, as the teardown of a repository does:
/// swaps each mapping from the id it holds to zero, whatever name the branch's metadata holds, and
/// removes the latest and metadata keys of that id. A mapping that changed since the listing is
/// left as it is. A write that fails is logged and the rest go on; a listing that fails removes
/// nothing.
pub async fn purge_mapped_branches(repository: Arc<RepositoryContext>) {
    let listed = match repository
        .read_mutable_store()
        .list(repository.id, KeyType::BranchId)
        .await
    {
        Ok(listed) => listed,
        Err(err) => {
            lore_debug!(
                "Failed to list the branch names of {}: {err}",
                repository.id
            );
            return;
        }
    };
    let mappings: Vec<(Hash, Hash)> = UnboundedReceiverStream::new(listed.channel())
        .collect()
        .await;
    for (key, id) in mappings {
        let branch = id.to_context();
        if let Err(err) = mutable_try_store_key(
            repository.clone(),
            key,
            KeyType::BranchId,
            id,
            Hash::default(),
        )
        .await
        {
            lore_debug!("Failed to release a name of branch {branch}: {err}");
        }
        for function in [LATEST, METADATA] {
            if let Err(err) = mutable_delete(repository.clone(), function, branch).await {
                lore_debug!("Failed to remove the {function} key of branch {branch}: {err}");
            }
        }
    }
}

/// The records of the live branches of `repository` at the remote.
pub async fn list_remote(
    remote: Arc<Connection>,
    repository: RepositoryId,
) -> Result<Vec<BranchRecord>, BranchError> {
    let remote = remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;

    let response = remote
        .branch_list(None, false)
        .await
        .forward::<BranchError>("Failed to list branches on remote")?;

    Ok(response.list)
}

/// Return `None` when remote enumeration exceeds the caller's branch budget.
pub(crate) async fn list_remote_bounded(
    remote: Arc<Connection>,
    repository: RepositoryId,
    limit: usize,
) -> Result<Option<Vec<BranchRecord>>, BranchError> {
    let remote = remote
        .revision(repository)
        .await
        .forward::<BranchError>("Failed to connect to remote revision service")?;
    let response = remote
        .branch_list_bounded(limit)
        .await
        .forward::<BranchError>("Failed to list branches on remote")?;
    Ok(response.map(|response| response.list))
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
    lore_debug!(
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

    lore_debug!(
        "Revision diff base {base_revision} source {source_revision} target {target_revision}"
    );

    let summary = Diff3Summary {
        base: base_revision,
        source: source_revision,
        target: target_revision,
    };

    let (inner_tx, inner_rx) = mpsc::channel::<Result<DiffItem, StateError>>(256);
    let driver = std::pin::pin!(revision::diff3_with_source_cap(
        repository,
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
    relay_revision_diff3(driver, inner_rx, auto_resolve, &tx).await?;

    Ok(summary)
}

/// Relays the items of the three-way diff `driver` makes through
/// [`emit_diff_item_with_auto_resolve`].
///
/// A function of its own because its items live across several awaits: kept in
/// [`diff3_with_source_cap`] they would take space in its future while the base
/// is resolved as well. The caller pins the diff, which takes the caller's
/// arguments rather than a copy of them. Each item is relayed after the
/// `select!` that received it, and a diff that outlives the channel is awaited
/// after the loop, so no item is held twice.
#[lore_macro::test_pub]
async fn relay_revision_diff3(
    mut driver: Pin<&mut impl Future<Output = Result<Diff3Summary, StateError>>>,
    mut inner_rx: mpsc::Receiver<Result<DiffItem, StateError>>,
    auto_resolve: bool,
    tx: &mpsc::Sender<Result<DiffItem, BranchError>>,
) -> Result<(), BranchError> {
    let mut driven = false;
    loop {
        let item = tokio::select! {
            biased;
            item = inner_rx.recv() => match item {
                Some(item) => item,
                None => break,
            },
            result = &mut driver, if !driven => {
                result.forward::<BranchError>("Failed to calculate branch diff")?;
                driven = true;
                continue;
            }
        };
        let item = item.forward::<BranchError>("Failed to calculate branch diff")?;
        emit_diff_item_with_auto_resolve(item, auto_resolve, tx).await?;
    }
    if !driven {
        driver
            .await
            .forward::<BranchError>("Failed to calculate branch diff")?;
    }

    Ok(())
}

/// Per-`DiffItem` step of `branch::diff3`'s auto-resolve drain. Kept
/// sequential: parallelising without a concurrency cap breaks the
/// streaming pipeline's memory bound — each in-flight conflict pins
/// two `NodeChange`s and three open temp files until the text-merge
/// completes.
///
/// The text merge is boxed. Only a conflict with auto-resolve on reaches it,
/// and inline it would make the step as large as the merge for every item.
///
/// Not an `async fn`, which would hold a second copy of its arguments. A
/// resolved conflict replaces the item in place, so the item is held once.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn emit_diff_item_with_auto_resolve(
    mut item: DiffItem,
    auto_resolve: bool,
    tx: &mpsc::Sender<Result<DiffItem, BranchError>>,
) -> impl Future<Output = Result<(), BranchError>> + '_ {
    async move {
        if auto_resolve
            && let DiffItem::Conflict(pair) = &item
            && let Some(resolved) = Box::pin(try_auto_resolve_conflict(&pair.0, &pair.1)).await?
        {
            item = DiffItem::Change(resolved);
        }
        let permit = tx
            .reserve()
            .await
            .map_err(|_closed| Internal::msg("diff3 channel closed"))?;
        permit.send(Ok(item));
        Ok(())
    }
}

/// Realises the three sides of one conflict into temp files and runs
/// `merge3_text_by_pathbuf`. Returns `Some(resolved_change)` only when
/// the merge produces no conflict markers — any merge failure or any
/// markers in the output preserve the conflict (returns `None`). The
/// resolved change carries the executable bit [`change::merged_mode`] gives
/// the file.
#[lore_macro::test_pub]
async fn try_auto_resolve_conflict(
    change_from: &NodeChange,
    change_to: &NodeChange,
) -> Result<Option<NodeChange>, BranchError> {
    if change_from.path() != change_to.path() {
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

    if change_from.to.mapping.node.is_valid_node_id() {
        lore_trace!("Change from theirs has valid to node, realize theirs file {theirs_file}");
        let node = change_from
            .to
            .mapping
            .state
            .block(
                change_from.to.mapping.repository.clone(),
                NodeBlock::index(change_from.to.mapping.node),
            )
            .await
            .forward::<BranchError>("Failed to deserialize revisions state")?
            .node(Node::index(change_from.to.mapping.node));
        // TODO(vri): UCS-19228 - Links: Realize link node files during branch sync
        if node.is_file() {
            if sync::realize_scratch_file(
                change_from.to.mapping.repository.clone(),
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

    if !crate::infer::infer_is_diffable(&lore_storage::ContentSource::file(&theirs_path))
        .await
        .unwrap_or(false)
    {
        lore_trace!("Change is not diffable and cannot be auto resolved, continue");
        return Ok(None);
    }

    if change_from.from.mapping.node.is_valid_node_id() {
        lore_trace!("Change from base has valid from node, realize base file {base_file}");
        let node = change_from
            .from
            .mapping
            .state
            .block(
                change_from.from.mapping.repository.clone(),
                NodeBlock::index(change_from.from.mapping.node),
            )
            .await
            .forward::<BranchError>("Failed to deserialize revisions state")?
            .node(Node::index(change_from.from.mapping.node));
        // TODO(vri): UCS-19228 - Links: Realize link node files during branch sync
        if node.is_file() {
            if sync::realize_scratch_file(
                change_from.from.mapping.repository.clone(),
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

    if change_to.to.mapping.node.is_valid_node_id() {
        lore_trace!("Change to mine has valid from node, realize mine file {mine_file}");
        let node = change_to
            .to
            .mapping
            .state
            .block(
                change_to.to.mapping.repository.clone(),
                NodeBlock::index(change_to.to.mapping.node),
            )
            .await
            .forward::<BranchError>("Failed to deserialize revisions state")?
            .node(Node::index(change_to.to.mapping.node));
        // TODO(vri): UCS-19228 - Links: Realize link node files during branch sync
        if node.is_file() {
            if sync::realize_scratch_file(
                change_to.to.mapping.repository.clone(),
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
        let mut to = change_to.to.clone();
        if let Some(mode) = change::merged_mode(change_from, change_to) {
            to.mode = mode;
        }
        Ok(Some(NodeChange {
            action: change_to.action,
            flags: change_to.flags | change::Flags::ConflictAutomerged,
            from: change_to.from.clone(),
            to,
        }))
    } else {
        Ok(None)
    }
}

/// The stack of `branch` from its metadata, read locally or on the remote, and empty when it can't
/// be read. The remote's refusal of the caller is returned rather than read as no stack. `side`
/// names the branch in the log.
async fn diff3_stack(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    side: &str,
) -> Result<Vec<BranchPoint>, BranchError> {
    let Some(branch_metadata) = absent_unless_refused::<_, _, BranchError>(
        metadata(repository.clone(), branch).await,
        "loading branch metadata",
    )?
    else {
        lore_debug!("No local or remote {side} branch metadata available");
        return Ok(Vec::new());
    };
    let stack = stack(&branch_metadata);
    lore_debug!("Loaded metadata for {side} branch {branch}, found stack {stack:?}");
    Ok(stack)
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
        let source_stack = diff3_stack(&repository, source_branch, "source").await?;
        let target_stack = diff3_stack(&repository, target_branch, "target").await?;

        let Some(common_ancestor) = find_common_ancestor_from_branch_points(
            repository.clone(),
            source_branch,
            source_revision,
            &source_stack,
            target_branch,
            target_revision,
            &target_stack,
        )
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
        let branch_point = diff3_stack(&repository, source_branch, "source")
            .await?
            .first()
            .map(|parent| parent.revision);
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
/// `MAX_DIVERGENT_HISTORY_LENGTH` revisions. It fails where a batch of the line
/// fails to load, as [`find::batch_load_history`] describes.
///
/// The two ways of not finding it are worth telling apart.
/// [`HistoryLineSearch::Diverged`] is a result: the line went past where the
/// revision would have been. [`HistoryLineSearch::Exhausted`] is an absence of
/// one, and searching the two lines for where they meet - which is bounded the
/// same way - cannot do better from there.
#[lore_macro::test_pub]
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

    let mut line = load_history_line(repository.clone(), newer_revision).await?;
    let mut scanned = 0;

    loop {
        if line[scanned..]
            .iter()
            .any(|&(_, revision)| revision == older_revision)
        {
            lore_debug!(
                "Revision {older_revision} -> {older_revision_number} is on the line from {newer_revision}"
            );
            return Ok(HistoryLineSearch::Reached);
        }
        scanned = line.len();

        if history_reached_floor(&line, older_revision_number) {
            lore_debug!(
                "Line from {newer_revision} passed revision number {older_revision_number} without reaching {older_revision}"
            );
            return Ok(HistoryLineSearch::Diverged);
        }

        if line.len() >= MAX_DIVERGENT_HISTORY_LENGTH {
            lore_warn!(
                "Reached maximum history depth of {MAX_DIVERGENT_HISTORY_LENGTH} following {newer_revision} back to {older_revision}"
            );
            return Ok(HistoryLineSearch::Exhausted);
        }

        if load_additional_history(repository.clone(), &mut line).await? {
            lore_debug!(
                "Line from {newer_revision} ran out without reaching {older_revision} -> {older_revision_number}"
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
/// The lines meet as [`history::meet`] describes, each loaded a batch at a time
/// and capped at `MAX_DIVERGENT_HISTORY_LENGTH`. A batch is loaded only for a
/// line `meet` waits on, with every revision passed, which is the line whose
/// oldest revision has the higher number, or both at equal numbers. `Ok(None)`
/// means a line ended, or reached the cap, before the lines met. A line whose
/// revision numbers do not decrease fails the search, as does a batch that fails
/// to load, as [`find::batch_load_history`] describes.
#[lore_macro::test_pub]
async fn find_common_revision_in_history_lines(
    repository: Arc<RepositoryContext>,
    self_revision: Hash,
    other_revision: Hash,
) -> Result<Option<Hash>, BranchError> {
    lore_debug!("Follow {self_revision} and {other_revision} back until the lines meet");

    // Both lines are fetched at once. Each is a round trip when the history is not
    // cached, and neither depends on the other.
    let (mut self_line, mut other_line) = try_join!(
        load_history_line(repository.clone(), self_revision),
        load_history_line(repository.clone(), other_revision)
    )?;

    let mut self_ended = false;
    let mut other_ended = false;
    let mut self_loaded = 0;
    let mut other_loaded = 0;
    let mut passed = (0, 0);

    loop {
        for (line, loaded) in [(&self_line, self_loaded), (&other_line, other_loaded)] {
            if let Some((number, revision)) =
                history::out_of_order(|index| line.get(index).copied(), loaded)
            {
                return Err(BranchError::internal(format!(
                    "the history line is out of order at revision {revision}, numbered {number}"
                )));
            }
        }
        self_loaded = self_line.len();
        other_loaded = other_line.len();

        if let Some((shared, _)) = history::meet(
            |index| self_line.get(index).copied(),
            |index| other_line.get(index).copied(),
            &mut passed,
        ) {
            return Ok(self_line.get(shared).map(|&(_, revision)| revision));
        }

        let self_waits = passed.0 == self_line.len();
        let other_waits = passed.1 == other_line.len();
        if (self_waits && self_ended) || (other_waits && other_ended) {
            return Ok(None);
        }
        if (self_waits && self_line.len() >= MAX_DIVERGENT_HISTORY_LENGTH)
            || (other_waits && other_line.len() >= MAX_DIVERGENT_HISTORY_LENGTH)
        {
            lore_warn!(
                "Reached maximum history depth of {MAX_DIVERGENT_HISTORY_LENGTH} following {self_revision} and {other_revision} back to a shared revision"
            );
            return Ok(None);
        }

        // Two round trips that need not wait on each other, where both lines wait.
        try_join!(
            async {
                if self_waits {
                    self_ended =
                        load_additional_history(repository.clone(), &mut self_line).await?;
                }
                Ok::<_, BranchError>(())
            },
            async {
                if other_waits {
                    other_ended =
                        load_additional_history(repository.clone(), &mut other_line).await?;
                }
                Ok::<_, BranchError>(())
            }
        )?;
    }
}

/// Load a line of revisions from `revision` backward through self parents, each
/// with its revision number. Falls back to a walk that reads each state as
/// `repository` reads it, from the remote where it has one, when the batch load
/// comes back empty, as where the store lacks `revision` and the remote cannot
/// list its line. Fails where the batch load does, as [`find::batch_load_history`]
/// describes.
#[lore_macro::test_pub]
async fn load_history_line(
    repository: Arc<RepositoryContext>,
    revision: Hash,
) -> Result<Vec<(u64, Hash)>, BranchError> {
    let mut history = find::batch_load_history(repository.clone(), revision)
        .await
        .forward::<BranchError>("loading the first batch of a history line")?;
    if !history.is_empty() {
        // The caller extends this a batch at a time up to the search depth, so
        // take the growth in one allocation rather than a copy per doubling.
        history.reserve(MAX_DIVERGENT_HISTORY_LENGTH.saturating_sub(history.len()));
        return Ok(history);
    }

    lore_debug!(
        "Found no revision from {revision} in a batch, reading the line's states one at a time"
    );

    let mut history = Vec::new();
    let Ok(mut state_iter) = state::State::deserialize(repository.clone(), revision).await else {
        return Ok(history);
    };
    history.push((state_iter.revision_number(), revision));

    while history.len() < find::BATCH_COUNT && !state_iter.parent_self().is_zero() {
        let revision_next = state_iter.parent_self();
        let Ok(state_next) = state::State::deserialize(repository.clone(), revision_next).await
        else {
            break;
        };
        history.push((state_next.revision_number(), revision_next));
        state_iter = state_next;
    }

    Ok(history)
}

/// Whether the oldest revision of `history` is numbered at or below
/// `floor_revision_number`. An empty line counts as reached.
#[lore_macro::test_pub]
fn history_reached_floor(history: &[(u64, Hash)], floor_revision_number: u64) -> bool {
    history
        .last()
        .is_none_or(|&(number, _)| number <= floor_revision_number)
}

/// Extend a line with the next batch of older revisions. Returns whether the line
/// ran out: nothing below its oldest revision could be loaded. Fails where the
/// batch load does, as [`find::batch_load_history`] describes.
#[lore_macro::test_pub]
async fn load_additional_history(
    repository: Arc<RepositoryContext>,
    history: &mut Vec<(u64, Hash)>,
) -> Result<bool, BranchError> {
    let Some(&(_, last)) = history.last() else {
        return Ok(true);
    };

    let additional = find::batch_load_history(repository.clone(), last)
        .await
        .forward::<BranchError>("loading the next batch of a history line")?;
    let fresh = find::below(&additional, last);
    if fresh.is_empty() {
        return Ok(true);
    }

    history.extend_from_slice(fresh);
    Ok(false)
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
#[lore_macro::test_pub]
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

/// Send the event that begins a branch diff, reporting the resolved branches
/// and revisions being compared. Sent before the diff runs, so it precedes
/// the diff's own diagnostics in the event stream.
pub fn dispatch_diff_events(
    diff: &DiffResult,
    source_branch: BranchId,
    source_branch_name: &str,
    target_branch: BranchId,
    target_branch_name: &str,
) {
    event::LoreEvent::BranchDiffBegin(LoreBranchDiffBeginEventData {
        source_branch,
        source_branch_name: source_branch_name.into(),
        source_revision: diff.source,
        target_branch,
        target_branch_name: target_branch_name.into(),
        target_revision: diff.target,
        base_revision: diff.base,
    })
    .send();

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
