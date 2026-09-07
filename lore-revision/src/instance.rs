// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_storage::store_types::KeyType;
use serde::Deserialize;
use serde::Serialize;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::anchor::AnchorError;
use crate::errors::AddressNotFound;
use crate::errors::Disconnected;
use crate::errors::FileNotFound;
use crate::errors::InvalidPath;
use crate::errors::LinkNotFound;
use crate::errors::Maintenance;
use crate::errors::NoRemote;
use crate::errors::NodeNotFound;
use crate::errors::NotAuthenticated;
use crate::errors::NotAuthorized;
use crate::errors::NotConnected;
use crate::errors::NotFound;
use crate::errors::NotSupported;
use crate::errors::Oversized;
use crate::errors::PayloadNotFound;
use crate::errors::SlowDown;
use crate::errors::WriteRequired;
use crate::event::EventError;
use crate::hash;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore_debug;
use crate::lore_warn;
use crate::metadata::Metadata;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryContextCreationArgs;

pub const INSTANCE_METADATA: &str = "instance-metadata";
pub const ANCHOR_CURRENT: &str = "anchor-current";
pub const ANCHOR_CURRENT_BRANCH: &str = "anchor-current-branch";
pub const ANCHOR_STAGED: &str = "anchor-staged";

/// A unique identity for a repository instance (a local checkout).
///
/// Each instance gets a stable `UUIDv7` generated once at creation time
/// and stored in `.lore/instance`. The instance ID is used to derive
/// per-instance anchor keys in the mutable store, distinguishing one
/// instance's checkout state from another when sharing a shared store.
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    IntoBytes,
    FromBytes,
    Immutable,
    Serialize,
    Deserialize,
)]
pub struct InstanceId {
    /// The raw 16-byte identifier
    data: [u8; 16],
}

impl InstanceId {
    /// Generate a new unique instance ID using `UUIDv7`.
    pub fn generate() -> Self {
        let bytes = uuid::Uuid::now_v7().into_bytes();
        Self { data: bytes }
    }

    pub fn is_zero(&self) -> bool {
        self.data == [0u8; 16]
    }

    pub fn data(&self) -> &[u8; 16] {
        &self.data
    }

    /// Read an instance ID from the `.lore/instance` file.
    pub fn read_from_file(path: PathBuf) -> io::Result<Self> {
        let mut id = Self::default();
        // Synchronous read: config file, avoids thread hop and queuing behind
        // any store flush tasks still in flight from the previous command.
        std::io::Read::read_exact(&mut std::fs::File::open(path)?, id.as_mut_bytes())?;
        Ok(id)
    }

    /// Write an instance ID to the `.lore/instance` file.
    pub async fn write_to_file(&self, path: PathBuf) -> io::Result<()> {
        lore_io::IoDriver::global()
            .write_file_bytes(path, bytes::Bytes::copy_from_slice(self.as_bytes()), false)
            .await?;
        Ok(())
    }

    pub fn text_encoding(&self) -> String {
        hex::encode(self.data())
    }
}

/// Derive the mutable store key for an instance's metadata entry.
///
/// The value stored at this key is the hash of the instance metadata blob
/// in the immutable store (containing instance ID, path, and creation timestamp).
pub fn instance_key(salt: &[u8], instance: InstanceId) -> (Hash, KeyType) {
    let key = hash::hash_function_arg(salt, INSTANCE_METADATA, instance.text_encoding().as_str());
    (key, KeyType::Instance)
}

/// Derive the mutable store key for an instance's anchor.
///
/// The `function` parameter selects which anchor (`ANCHOR_CURRENT` or
/// `ANCHOR_STAGED`). The value stored at this key is the revision hash.
pub fn anchor_key(salt: &[u8], function: &str, instance: InstanceId) -> (Hash, KeyType) {
    let key = hash::hash_function_arg(salt, function, hex::encode(instance.data()).as_str());
    (key, KeyType::Untyped)
}

const PATH: &str = "path";
const CREATED: &str = "created";
const INSTANCE_ID: &str = "instance-id";

/// Instance metadata stored as a blob in the immutable store.
#[derive(Debug)]
pub struct InstanceMetadata {
    pub instance_id: InstanceId,
    pub path: String,
    pub created: u64,
}

/// Register an instance in the mutable store by writing its metadata to the
/// immutable store and storing the metadata hash under the instance key.
///
/// A path holds one checkout, and that checkout's `.lore/instance` names
/// `instance_id`, so any other instance still registered at `path` was left
/// behind when the directory was re-created on a mutable store that outlived
/// its `.lore` directory: `repository create --force` or a re-clone on a
/// shared store, or a checkout moved onto the path of a deleted one. Those
/// registrations are retired here, so the instance list never carries two
/// entries for one root directory.
pub async fn register_instance(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
    path: &str,
) -> Result<(), InstanceError> {
    let normalized_path = crate::util::path::clean(path.to_owned());
    store_normalized_registration(repository, instance_id, &normalized_path).await?;
    retire_instances_at_path(repository, instance_id, &normalized_path).await;
    Ok(())
}

/// Write the registration record for `instance_id` at `path` without retiring
/// other registrations at that path.
///
/// [`register_instance`] is the entry point that keeps one registration per
/// path. This is exposed so tests can build the duplicate state an earlier
/// client left behind and check that prune and recovery handle it.
#[doc(hidden)]
pub async fn store_instance_registration(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
    path: &str,
) -> Result<(), InstanceError> {
    let normalized_path = crate::util::path::clean(path.to_owned());
    store_normalized_registration(repository, instance_id, &normalized_path).await
}

/// Write the registration record for `instance_id`, whose `normalized_path` is
/// already in the form [`crate::util::path::clean`] produces.
async fn store_normalized_registration(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
    normalized_path: &str,
) -> Result<(), InstanceError> {
    let mut metadata = Metadata::new();
    metadata
        .set_string(INSTANCE_ID, hex::encode(instance_id.data()).as_str())
        .forward_any::<InstanceError>("failed to set instance ID in metadata")?;
    metadata
        .set_string(PATH, normalized_path)
        .forward_any::<InstanceError>("failed to set path in metadata")?;
    metadata
        .set_u64(CREATED, crate::util::time::timestamp())
        .forward_any::<InstanceError>("failed to set created in metadata")?;

    let metadata_hash = metadata
        .serialize_local(repository.clone())
        .await
        .forward_any::<InstanceError>("failed to serialize instance metadata")?;

    let (key, key_type) = instance_key(repository.salt(), instance_id);
    let handle = repository.try_write_mutable_store().ok_or(WriteRequired)?;
    handle
        .store(repository.id, key, metadata_hash, key_type)
        .await
        .forward::<InstanceError>("failed to store instance registration")?;

    lore_debug!("Registered instance {instance_id} with metadata hash {metadata_hash}");
    Ok(())
}

/// Whether `path` names the location `normalized` already names, normalizing
/// into `scratch` rather than allocating for each path compared.
fn names_same_path(path: &str, normalized: &str, scratch: &mut String) -> bool {
    scratch.clear();
    scratch.push_str(path);
    crate::util::path::clean_in_place(scratch);
    scratch == normalized
}

/// Retire every registration other than `current` that records
/// `normalized_path`, which is already in the form
/// [`crate::util::path::clean`] produces.
///
/// Run by [`register_instance`] once its own record is written, so a path the
/// registering checkout owns is left holding one entry without waiting for a
/// reader to notice.
///
/// Best-effort: a failure to enumerate leaves the entries for
/// [`instance_prune`], which classifies them as superseded.
async fn retire_instances_at_path(
    repository: &Arc<RepositoryContext>,
    current: InstanceId,
    normalized_path: &str,
) {
    let instances = match list_instances(repository).await {
        Ok(instances) => instances,
        Err(err) => {
            lore_warn!("Failed to list instances while registering {current}: {err}");
            return;
        }
    };
    let mut scratch = String::new();
    for instance in instances {
        if instance.instance_id == current
            || instance.instance_id.is_zero()
            || !names_same_path(&instance.path, normalized_path, &mut scratch)
        {
            continue;
        }
        lore_debug!(
            "Retiring instance {} registered at {normalized_path}, superseded by {current}",
            instance.instance_id
        );
        remove_instance_keys(repository, instance.instance_id).await;
    }
}

/// Zero the registration and anchor keys of `instance_id`, removing it from the
/// store. Reports whether the write was possible: a read-only caller leaves the
/// entry to the next writer. Failures are otherwise ignored, since a key that
/// could not be zeroed is picked up by the next prune.
async fn remove_instance_keys(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
) -> bool {
    let Some(handle) = repository.try_write_mutable_store() else {
        return false;
    };
    let (key, key_type) = instance_key(repository.salt(), instance_id);
    let _ = handle
        .store(repository.id, key, Hash::default(), key_type)
        .await;
    for function in [ANCHOR_CURRENT, ANCHOR_CURRENT_BRANCH, ANCHOR_STAGED] {
        let (key, key_type) = anchor_key(repository.salt(), function, instance_id);
        let _ = handle
            .store(repository.id, key, Hash::default(), key_type)
            .await;
    }
    true
}

/// A registration that no longer describes a live checkout.
struct StaleInstance {
    metadata: InstanceMetadata,
    staleness: InstanceStaleness,
    /// The anchor state read before retirement, absent while the registration
    /// is left in place and its own anchors still answer.
    retired_anchors: Option<(BranchId, String, Hash)>,
}

/// Which stale registrations a reader retires as it reads.
enum RetirePolicy {
    /// Only one the path's own `.lore/instance` contradicts. A path that holds
    /// no checkout may be a filesystem that is not mounted at the moment, so
    /// clearing it is left to an explicit prune.
    Contradicted,
    /// Every registration that describes no live checkout.
    Stale,
}

/// What a caller does with the stale registrations.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StaleReport {
    /// Nothing, so none are returned and no anchor state is read.
    Discard,
    /// Reports them, so all are returned, and the anchor state of one being
    /// retired is read before its keys are zeroed.
    Return,
}

/// The registrations describing a live checkout, and the stale ones `report`
/// asks for, retiring those `policy` covers.
///
/// Neither the current instance nor a registration without a usable ID is
/// classified: a checkout cannot conclude that it is itself gone, and an ID that
/// could not be read names no keys to remove. A read-only reader retires
/// nothing and reports what it found.
async fn reconcile_instances(
    repository: &Arc<RepositoryContext>,
    policy: RetirePolicy,
    report: StaleReport,
) -> Result<(Vec<InstanceMetadata>, Vec<StaleInstance>), InstanceError> {
    let instances = list_instances(repository).await?;
    let mut live = Vec::with_capacity(instances.len());
    let mut stale = Vec::new();
    let mut retired_any = false;
    for instance in instances {
        if instance.instance_id == repository.instance_id || instance.instance_id.is_zero() {
            live.push(instance);
            continue;
        }
        let staleness = instance_staleness(&instance.path, instance.instance_id).await;
        if !staleness.is_stale() {
            live.push(instance);
            continue;
        }

        let retire = match policy {
            RetirePolicy::Contradicted => staleness == InstanceStaleness::Superseded,
            RetirePolicy::Stale => true,
        };
        let mut retired_anchors = None;
        if retire {
            if report == StaleReport::Return {
                retired_anchors =
                    Some(instance_anchor_state(repository, instance.instance_id).await);
            }
            lore_debug!(
                "Retiring {staleness:?} instance {} registered at {}",
                instance.instance_id,
                instance.path
            );
            retired_any |= remove_instance_keys(repository, instance.instance_id).await;
        }
        if report == StaleReport::Return {
            stale.push(StaleInstance {
                metadata: instance,
                staleness,
                retired_anchors,
            });
        }
    }
    if retired_any {
        let _ = repository.flush(false).await;
    }
    Ok((live, stale))
}

/// Load instance metadata from the immutable store given the metadata hash.
pub async fn load_instance_metadata(
    repository: &Arc<RepositoryContext>,
    metadata_hash: Hash,
) -> Result<InstanceMetadata, InstanceError> {
    let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward_any::<InstanceError>("failed to deserialize instance metadata")?;

    // Missing or corrupt fields are non-fatal — instance metadata is advisory
    // (used for branch checkout warnings and stale instance detection), not
    // required for correctness. Default values degrade gracefully: an empty
    // path causes the instance to appear stale, zero timestamp is harmless,
    // and a zero instance ID means the ID can be recovered from the mutable
    // store key instead.
    let instance_id = metadata
        .get_string(INSTANCE_ID)
        .ok()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|bytes| {
            let mut id = InstanceId::default();
            if bytes.len() == 16 {
                id.as_mut_bytes().copy_from_slice(&bytes);
                Some(id)
            } else {
                None
            }
        })
        .unwrap_or_default();
    let path = metadata
        .get_string(PATH)
        .map(|s| s.to_string())
        .unwrap_or_default();
    let created = metadata.get_u64(CREATED).unwrap_or_default();

    Ok(InstanceMetadata {
        instance_id,
        path,
        created,
    })
}

/// Attempt to recover a lost instance ID by enumerating all registered
/// instances and matching by filesystem path. Returns `Some(id)` if an
/// existing instance entry has a path matching `current_path`; the most
/// recently registered one when several do.
pub async fn recover_instance_id(
    repository_id: lore_storage::Partition,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    current_path: &str,
) -> Option<InstanceId> {
    use futures::StreamExt;

    let mut stream = mutable_store
        .clone()
        .list(repository_id, KeyType::Instance)
        .await
        .ok()?;

    // Build a temporary repository context for metadata deserialization
    let temp_repo = Arc::new(RepositoryContext::new(RepositoryContextCreationArgs {
        paths: None,
        immutable_store,
        mutable_store,
        id: repository_id,
        instance_id: InstanceId::default(),
        remote: Err(lore_transport::ProtocolError::from(crate::errors::NoRemote)),
        filter: Arc::default(),
        filesystem_provider: None,
    }));

    let normalized_current = crate::util::path::clean(current_path.to_owned());

    let mut newest: Option<InstanceMetadata> = None;
    let mut scratch = String::new();
    while let Some((_key, metadata_hash)) = stream.next().await {
        if metadata_hash.is_zero() {
            continue;
        }
        if let Ok(metadata) = load_instance_metadata(&temp_repo, metadata_hash).await
            && !metadata.instance_id.is_zero()
            && names_same_path(&metadata.path, &normalized_current, &mut scratch)
            && newest.as_ref().is_none_or(|best| {
                (metadata.created, metadata.instance_id) > (best.created, best.instance_id)
            })
        {
            newest = Some(metadata);
        }
    }

    newest.map(|metadata| {
        lore_debug!(
            "Recovered instance ID {} from path match",
            metadata.instance_id
        );
        metadata.instance_id
    })
}

/// List all registered instances for a repository by querying the mutable store.
///
/// Returns a list of `InstanceMetadata` for each registered instance.
pub async fn list_instances(
    repository: &Arc<RepositoryContext>,
) -> Result<Vec<InstanceMetadata>, InstanceError> {
    use futures::StreamExt;

    let mut stream = repository
        .read_mutable_store()
        .list(repository.id, KeyType::Instance)
        .await
        .forward::<InstanceError>("failed to list instances")?;

    let mut instances = Vec::new();
    while let Some((_key, metadata_hash)) = stream.next().await {
        if metadata_hash.is_zero() {
            continue;
        }
        instances.push(load_instance_metadata(repository, metadata_hash).await?);
    }
    Ok(instances)
}

/// Check if any other active instance has the given branch checked out.
///
/// Returns the list of active instances on that branch (excluding self). A
/// stale registration is never returned, and is retired where the path
/// contradicts it.
pub async fn instances_on_branch(
    repository: &Arc<RepositoryContext>,
    target_branch: crate::lore::BranchId,
) -> Result<Vec<InstanceMetadata>, InstanceError> {
    let (live, _stale) =
        reconcile_instances(repository, RetirePolicy::Contradicted, StaleReport::Discard).await?;
    let self_id = repository.instance_id;

    let mut matches = Vec::new();
    for instance in live {
        if instance.instance_id == self_id {
            continue;
        }

        // Load the branch from the ANCHOR_CURRENT_BRANCH key — the
        // authoritative source of which branch an instance is on.
        let (branch_key, branch_key_type) = anchor_key(
            repository.salt(),
            ANCHOR_CURRENT_BRANCH,
            instance.instance_id,
        );
        let branch = match repository
            .read_mutable_store()
            .load(repository.id, branch_key, branch_key_type)
            .await
        {
            Ok(hash) if !hash.is_zero() => hash.to_context(),
            _ => continue,
        };

        if branch == target_branch {
            matches.push(instance);
        }
    }
    Ok(matches)
}

/// Event data warning that several instances share the same checked-out branch.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMultipleInstanceEventData {
    /// The branch checked out by more than one instance
    pub branch: BranchId,
    /// Identifiers of the other instances on the branch
    pub instance_ids: crate::interface::LoreArray<InstanceId>,
    /// Filesystem paths of the other instances on the branch
    pub instance_paths: crate::interface::LoreArray<LoreString>,
}

/// Emit a `BranchMultipleInstance` warning event if other active instances
/// have the given branch checked out. Call during branch switch and sync
/// branch changes.
pub async fn warn_branch_multiple_instance(
    repository: &Arc<RepositoryContext>,
    target_branch: BranchId,
) {
    if let Ok(others) = instances_on_branch(repository, target_branch).await
        && !others.is_empty()
    {
        let ids: Vec<InstanceId> = others.iter().map(|m| m.instance_id).collect();
        let paths: Vec<LoreString> = others
            .iter()
            .map(|m| LoreString::from_str(&m.path))
            .collect();
        crate::event::LoreEvent::BranchMultipleInstance(LoreBranchMultipleInstanceEventData {
            branch: target_branch,
            instance_ids: crate::interface::LoreArray::from_vec(ids),
            instance_paths: crate::interface::LoreArray::from_vec(paths),
        })
        .send();
    }
}

/// Load the current anchor for this instance from the mutable store.
///
/// The revision comes from `ANCHOR_CURRENT`, the branch from
/// `ANCHOR_CURRENT_BRANCH`. If the branch key exists but the revision
/// is zero, the repository has no commits yet (fresh repo after create).
pub async fn load_current_anchor(
    repository: &Arc<RepositoryContext>,
) -> Result<(Hash, BranchId), AnchorError> {
    let (rev_key, rev_key_type) =
        anchor_key(repository.salt(), ANCHOR_CURRENT, repository.instance_id);
    let revision = repository
        .read_mutable_store()
        .load(repository.id, rev_key, rev_key_type)
        .await
        .ok()
        .filter(|h| !h.is_zero())
        .unwrap_or_default();

    let (branch_key, branch_key_type) = anchor_key(
        repository.salt(),
        ANCHOR_CURRENT_BRANCH,
        repository.instance_id,
    );
    let branch = repository
        .read_mutable_store()
        .load(repository.id, branch_key, branch_key_type)
        .await
        .ok()
        .filter(|h| !h.is_zero())
        .map(|h| h.to_context());

    if let Some(branch) = branch {
        return Ok((revision, branch));
    }

    // Fallback for pre-migration repositories: the anchor still lives in the
    // file-based `.urc/current` (32-byte revision + 16-byte branch). Migration
    // into the mutable store only runs in write-mode contexts, so a read-only
    // command on a repository with unmigrated anchors would otherwise fail.

    let dot_path = repository.dot_dir_path()?;
    let current_anchor_path = dot_path.join(crate::anchor::CURRENT);
    if current_anchor_path.exists()
        && let Ok((file_revision, file_branch)) =
            crate::anchor::deserialize_migrate_old(&current_anchor_path).await
    {
        lore_debug!("Loaded current anchor from legacy file (mutable store keys absent)");
        return Ok((file_revision, file_branch));
    }

    Err(AnchorError::internal("anchor branch is missing"))
}

/// Load the staged revision hash for this instance from the mutable store.
///
/// Returns `Ok(None)` if nothing is staged (zero hash or not found).
/// The staged state is always on the same branch as the current anchor,
/// so only the revision hash is returned — use `load_current_anchor()`
/// for the branch.
pub async fn load_staged_revision(
    repository: &Arc<RepositoryContext>,
) -> Result<Option<Hash>, AnchorError> {
    let (key, key_type) = anchor_key(repository.salt(), ANCHOR_STAGED, repository.instance_id);
    if let Ok(hash) = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        && !hash.is_zero()
    {
        return Ok(Some(hash));
    }

    // Fallback for pre-migration repositories: read the legacy `.urc/staged`
    // file (32-byte revision + 16-byte branch — branch is ignored here, the
    // staged anchor only carries a revision). Mirrors load_current_anchor's
    // file-based fallback for read-only commands on unmigrated repositories.
    let dot_path = repository.dot_dir_path()?;
    let staged_anchor_path = dot_path.join(crate::anchor::STAGED);
    if staged_anchor_path.exists()
        && let Ok((file_revision, _branch)) =
            crate::anchor::deserialize_migrate_old(&staged_anchor_path).await
        && !file_revision.is_zero()
    {
        lore_debug!("Loaded staged anchor from legacy file (mutable store key absent)");
        return Ok(Some(file_revision));
    }

    Ok(None)
}

/// Write the current anchor to the mutable store.
pub async fn store_current_anchor(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> Result<(), AnchorError> {
    let (key, key_type) = anchor_key(repository.salt(), ANCHOR_CURRENT, repository.instance_id);
    let handle = repository.try_write_mutable_store().ok_or(WriteRequired)?;
    handle
        .store(repository.id, key, revision, key_type)
        .await
        .forward_any::<AnchorError>("failed to store current anchor")?;
    Ok(())
}

/// Write the current branch to the mutable store (no flush).
///
/// Called during create, branch create, branch switch, and anchor migration.
/// Not called during commit — the branch is unchanged when committing.
pub async fn store_current_anchor_branch(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<(), AnchorError> {
    let (key, key_type) = anchor_key(
        repository.salt(),
        ANCHOR_CURRENT_BRANCH,
        repository.instance_id,
    );
    let handle = repository.try_write_mutable_store().ok_or(WriteRequired)?;
    handle
        .store(repository.id, key, Hash::from_context(branch), key_type)
        .await
        .forward_any::<AnchorError>("failed to store current anchor branch")?;
    Ok(())
}

/// Write the staged anchor to the mutable store (no flush).
pub async fn store_staged_anchor(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> Result<(), AnchorError> {
    let (key, key_type) = anchor_key(repository.salt(), ANCHOR_STAGED, repository.instance_id);
    let handle = repository.try_write_mutable_store().ok_or(WriteRequired)?;
    handle
        .store(repository.id, key, revision, key_type)
        .await
        .forward_any::<AnchorError>("failed to store staged anchor")?;
    Ok(())
}

/// Delete the staged anchor (write zero hash).
pub async fn delete_staged_anchor(repository: &Arc<RepositoryContext>) -> Result<(), AnchorError> {
    store_staged_anchor(repository, Hash::default()).await
}

#[error_set]
pub enum InstanceError {
    WriteRequired,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    Oversized,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    SlowDown,
    Maintenance,
    NotConnected,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotSupported,
}

impl EventError for InstanceError {
    fn translated(&self) -> LoreError {
        match self {
            InstanceError::Disconnected(_) => LoreError::Connection,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Whether a registered instance still describes a live checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceStaleness {
    /// The path exists and nothing there contradicts the registration.
    Active,
    /// The path no longer exists on disk.
    PathMissing,
    /// The path exists but its `.lore/instance` names a different instance:
    /// the directory was re-created or re-cloned and this registration was
    /// left behind.
    Superseded,
    /// The path exists but holds no readable `.lore/instance`, so no checkout
    /// the registration could describe.
    CheckoutMissing,
}

impl InstanceStaleness {
    pub fn is_stale(self) -> bool {
        self != Self::Active
    }

    /// The value [`LoreRepositoryInstanceEventData::stale`] carries.
    pub fn as_event_flag(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::PathMissing => 1,
            Self::Superseded => 2,
            Self::CheckoutMissing => 3,
        }
    }
}

/// Whether `error`, from examining a path, shows that the path is gone rather
/// than that it could not be reached.
fn is_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound && !names_unreachable_volume(error)
}

/// A drive letter carrying no volume and a network share that cannot be reached
/// both report the `NotFound` a deleted directory reports. The volume can come
/// back, so a registration on it is not evidence of a removed checkout.
#[cfg(target_family = "windows")]
fn names_unreachable_volume(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_INVALID_DRIVE as i32
                || code == windows_sys::Win32::Foundation::ERROR_BAD_NETPATH as i32
                || code == windows_sys::Win32::Foundation::ERROR_BAD_NET_NAME as i32
    )
}

/// No kernel here reports an unreachable volume as a missing path.
#[cfg(not(target_family = "windows"))]
fn names_unreachable_volume(_error: &io::Error) -> bool {
    false
}

/// Classify the registration of `instance_id` at `path`.
///
/// Only positive evidence makes a registration stale, since [`instance_prune`]
/// discards the anchors of what it retires. An absent path and an absent or
/// truncated instance file are such evidence; an empty path, a zero
/// `instance_id`, a mount the manager cannot inspect, an unreachable volume,
/// and a path or instance file that cannot be examined are not, and leave the
/// registration active.
///
/// The instance file is looked up through
/// [`crate::repository::get_dot_lore_path`], so a legacy `.urc` directory and
/// an SWFS mount whose `.lore` lives outside the path are both resolved.
pub async fn instance_staleness(path: &str, instance_id: InstanceId) -> InstanceStaleness {
    if path.is_empty() {
        return InstanceStaleness::Active;
    }
    let io = lore_io::IoDriver::global();
    match io.metadata(path).await {
        Ok(_) => {}
        Err(err) if is_absent(&err) => return InstanceStaleness::PathMissing,
        Err(_) => return InstanceStaleness::Active,
    }
    let Ok(dot_path) = crate::repository::get_dot_lore_path(Path::new(path)) else {
        return InstanceStaleness::Active;
    };
    let expected = instance_id.as_bytes();
    match io
        .read_file_bytes(dot_path.join(crate::repository::INSTANCE))
        .await
    {
        // Same read as `InstanceId::read_from_file`: the leading 16 bytes.
        Ok(bytes) if bytes.len() >= expected.len() => {
            if instance_id.is_zero() || bytes[..expected.len()] == *expected {
                InstanceStaleness::Active
            } else {
                InstanceStaleness::Superseded
            }
        }
        Ok(_) => InstanceStaleness::CheckoutMissing,
        Err(err) if is_absent(&err) => InstanceStaleness::CheckoutMissing,
        Err(_) => InstanceStaleness::Active,
    }
}

/// The branch (ID and name) and revision an instance has checked out, read
/// from its anchor keys. Zero or empty where an anchor is absent.
async fn instance_anchor_state(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
) -> (BranchId, String, Hash) {
    let (key, key_type) = anchor_key(repository.salt(), ANCHOR_CURRENT_BRANCH, instance_id);
    let branch_id = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()
        .filter(|h| !h.is_zero())
        .map(|h| h.to_context())
        .unwrap_or_default();
    let branch_name = if !branch_id.is_zero() {
        crate::branch::metadata(repository.clone(), branch_id)
            .await
            .ok()
            .and_then(|m| crate::branch::name(&m).ok().map(|s| s.to_string()))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let (key, key_type) = anchor_key(repository.salt(), ANCHOR_CURRENT, instance_id);
    let revision = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()
        .filter(|h| !h.is_zero())
        .unwrap_or_default();

    (branch_id, branch_name, revision)
}

use crate::event::LoreEvent;

/// Event data describing an instance — used for both listing and prune notifications.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceEventData {
    /// Identifier of the instance
    pub instance_id: InstanceId,
    /// Filesystem path of the instance
    pub path: LoreString,
    /// Name of the branch the instance has checked out
    pub branch_name: LoreString,
    /// Identifier of the branch the instance has checked out
    pub branch: BranchId,
    /// Current revision hash for the instance
    pub revision: Hash,
    /// Non-zero if the registration no longer describes a live checkout: 1 when
    /// the path no longer exists on disk, 2 when the path holds a repository
    /// whose `.lore/instance` names a different instance (superseded by a
    /// re-create or re-clone), 3 when the path holds no readable
    /// `.lore/instance` at all
    pub stale: u8,
}

/// Emit a `RepositoryInstance` event describing one registration.
fn send_instance_event(
    instance: &InstanceMetadata,
    staleness: InstanceStaleness,
    branch: BranchId,
    branch_name: &str,
    revision: Hash,
) {
    LoreEvent::RepositoryInstance(LoreRepositoryInstanceEventData {
        instance_id: instance.instance_id,
        path: LoreString::from_str(&instance.path),
        branch_name: LoreString::from_str(branch_name),
        branch,
        revision,
        stale: staleness.as_event_flag(),
    })
    .send();
}

/// Emit a `RepositoryInstance` event for a stale registration, reading the
/// anchors it still holds when they were not captured before retirement.
async fn send_stale_instance_event(repository: &Arc<RepositoryContext>, entry: &StaleInstance) {
    let read;
    let (branch, branch_name, revision) =
        if let Some((branch, branch_name, revision)) = entry.retired_anchors.as_ref() {
            (*branch, branch_name.as_str(), *revision)
        } else {
            read = instance_anchor_state(repository, entry.metadata.instance_id).await;
            (read.0, read.1.as_str(), read.2)
        };
    send_instance_event(
        &entry.metadata,
        entry.staleness,
        branch,
        branch_name,
        revision,
    );
}

/// List all registered instances, emitting events for each entry. The stale ones
/// are reported with their `stale` flag; being a read-only command, this leaves
/// removing them to [`instance_prune`].
pub async fn instance_list(repository: Arc<RepositoryContext>) -> Result<(), InstanceError> {
    let (live, stale) =
        reconcile_instances(&repository, RetirePolicy::Contradicted, StaleReport::Return).await?;
    for instance in &live {
        let (branch, branch_name, revision) =
            instance_anchor_state(&repository, instance.instance_id).await;
        send_instance_event(
            instance,
            InstanceStaleness::Active,
            branch,
            &branch_name,
            revision,
        );
    }
    for entry in &stale {
        send_stale_instance_event(&repository, entry).await;
    }
    Ok(())
}

/// Prune stale instances: those whose path no longer exists, those whose path
/// now holds a repository with a different current instance, and those whose
/// path holds no checkout at all.
/// Emits a `RepositoryInstance` event for each pruned instance.
pub async fn instance_prune(repository: Arc<RepositoryContext>) -> Result<u32, InstanceError> {
    if repository.try_write_token().is_none() {
        return Err(WriteRequired.into());
    }
    let (_live, stale) =
        reconcile_instances(&repository, RetirePolicy::Stale, StaleReport::Return).await?;
    for entry in &stale {
        send_stale_instance_event(&repository, entry).await;
    }
    Ok(u32::try_from(stale.len()).unwrap_or(u32::MAX))
}

/// Update the current instance's metadata path to match the working directory,
/// retiring any registration another instance left at that path.
pub async fn update_path(repository: Arc<RepositoryContext>) -> Result<(), InstanceError> {
    let current_path = repository.path_for_display().to_string();
    register_instance(&repository, repository.instance_id, &current_path).await?;
    lore_debug!("Updated instance path to {current_path}");
    Ok(())
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.data))
    }
}

impl fmt::Debug for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstanceId({})", hex::encode(self.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::SALT_LORE;

    #[test]
    fn instance_key_is_deterministic() {
        let id = InstanceId::generate();
        let (key1, typ1) = instance_key(SALT_LORE, id);
        let (key2, typ2) = instance_key(SALT_LORE, id);
        assert_eq!(key1, key2);
        assert_eq!(typ1, typ2);
        assert_eq!(typ1, KeyType::Instance);
    }

    #[test]
    fn instance_key_differs_for_different_ids() {
        let a = InstanceId::generate();
        let b = InstanceId::generate();
        let (key_a, _) = instance_key(SALT_LORE, a);
        let (key_b, _) = instance_key(SALT_LORE, b);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn anchor_keys_differ_for_current_vs_staged() {
        let id = InstanceId::generate();
        let (current, typ_c) = anchor_key(SALT_LORE, ANCHOR_CURRENT, id);
        let (staged, typ_s) = anchor_key(SALT_LORE, ANCHOR_STAGED, id);
        assert_ne!(current, staged);
        assert_eq!(typ_c, KeyType::Untyped);
        assert_eq!(typ_s, KeyType::Untyped);
    }

    #[test]
    fn generate_produces_nonzero_unique_values() {
        let a = InstanceId::generate();
        let b = InstanceId::generate();
        assert!(!a.is_zero());
        assert!(!b.is_zero());
        assert_ne!(a, b);
    }

    #[test]
    fn default_is_zero() {
        let id = InstanceId::default();
        assert!(id.is_zero());
    }

    #[test]
    fn roundtrip_bytes() {
        let id = InstanceId::generate();
        let bytes = id.as_bytes().to_vec();
        assert_eq!(bytes.len(), 16);
        let mut restored = InstanceId::default();
        restored.as_mut_bytes().copy_from_slice(&bytes);
        assert_eq!(id, restored);
    }

    #[test]
    fn display_is_hex() {
        let id = InstanceId::generate();
        let s = id.to_string();
        assert_eq!(s.len(), 32); // 16 bytes = 32 hex chars
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
