// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore_base::error::MountsBusy;
use lore_base::fs::lock::FSLock;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_io::clone::CloneStats;
use lore_storage::ReadOptions;
use lore_storage::StoreError;
use lore_storage::store_types::KeyType;
use serde::Serialize;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::anchor::AnchorError;
use crate::errors::AddressNotFound;
use crate::errors::Disconnected;
use crate::errors::FileNotFound;
use crate::errors::InvalidArguments;
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
use crate::fs::filesystem_provider::CloneAnswer;
use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::filesystem_provider::InstanceCloneError;
use crate::fs::filesystem_provider::clone_io_failure;
use crate::fs::filesystem_provider::lore_directory_within;
use crate::fs::mount_manager_state::MountManagerState;
use crate::global::external_dir::check_for_external_lore_dir;
use crate::global::external_dir::external_dir_for_instance;
use crate::hash;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore_debug;
use crate::lore_warn;
use crate::metadata::Metadata;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryContextCreationArgs;
use crate::repository::VfsType;
use crate::util::to_hex_str;

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
    bitcode::Encode,
    bitcode::Decode,
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

    /// Parse the hex form produced by [`Self::text_encoding`], as found in a FUSE mount's
    /// `lore:<instance-id-hex>` source string. Returns `None` for malformed or wrong-length input.
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = hex::decode(text).ok()?;
        let data: [u8; 16] = bytes.try_into().ok()?;
        Some(Self { data })
    }
}

/// Derive the mutable store key for an instance's metadata entry.
///
/// The value stored at this key is the hash of the instance metadata blob
/// in the immutable store (containing instance ID, path, and creation timestamp).
pub fn instance_key(salt: &[u8], instance: InstanceId) -> (Hash, KeyType) {
    (
        hash_instance_function(salt, INSTANCE_METADATA, instance),
        KeyType::Instance,
    )
}

/// Derive the mutable store key for an instance's anchor.
///
/// The `function` parameter selects which anchor (`ANCHOR_CURRENT` or
/// `ANCHOR_STAGED`). The value stored at this key is the revision hash.
pub fn anchor_key(salt: &[u8], function: &str, instance: InstanceId) -> (Hash, KeyType) {
    (
        hash_instance_function(salt, function, instance),
        KeyType::Untyped,
    )
}

/// Hash `salt`, `function` and the lower-case hex of `instance`, encoding the hex on the stack.
fn hash_instance_function(salt: &[u8], function: &str, instance: InstanceId) -> Hash {
    let mut digits = [0u8; 32];
    hash::hash_function_arg(salt, function, to_hex_str(instance.data(), &mut digits))
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
) -> Result<(), InstanceRegistryError> {
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
) -> Result<(), InstanceRegistryError> {
    let normalized_path = crate::util::path::clean(path.to_owned());
    store_normalized_registration(repository, instance_id, &normalized_path).await
}

/// Write the registration record for `instance_id`, whose `normalized_path` is
/// already in the form [`crate::util::path::clean`] produces.
async fn store_normalized_registration(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
    normalized_path: &str,
) -> Result<(), InstanceRegistryError> {
    let mut metadata = Metadata::new();
    metadata
        .set_string(INSTANCE_ID, hex::encode(instance_id.data()).as_str())
        .forward::<InstanceRegistryError>("failed to set instance ID in metadata")?;
    metadata
        .set_string(PATH, normalized_path)
        .forward::<InstanceRegistryError>("failed to set path in metadata")?;
    metadata
        .set_u64(CREATED, crate::util::time::timestamp())
        .forward::<InstanceRegistryError>("failed to set created in metadata")?;

    let metadata_hash = metadata
        .serialize_local(repository.clone())
        .await
        .forward::<InstanceRegistryError>("failed to serialize instance metadata")?;

    let (key, key_type) = instance_key(repository.salt(), instance_id);
    let handle = repository.try_write_mutable_store().ok_or(WriteRequired)?;
    handle
        .store(repository.id, key, metadata_hash, key_type)
        .await
        .forward::<InstanceRegistryError>("failed to store instance registration")?;

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
    let instances = list_instances(repository)
        .await
        .forward::<InstanceError>("listing registered instances")?;
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
        let _ = repository.flush(FlushOptions::default()).await;
    }
    Ok((live, stale))
}

/// Load instance metadata from the immutable store given the metadata hash.
pub async fn load_instance_metadata(
    repository: &Arc<RepositoryContext>,
    metadata_hash: Hash,
) -> Result<InstanceMetadata, InstanceRegistryError> {
    let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<InstanceRegistryError>("failed to deserialize instance metadata")?;
    Ok(instance_metadata_of(&metadata))
}

/// The registration that the blob `metadata` holds. A missing or corrupt field reads as its
/// default, and [`instance_staleness`] reads an empty path as active.
fn instance_metadata_of(metadata: &Metadata) -> InstanceMetadata {
    InstanceMetadata {
        instance_id: registered_id(metadata),
        path: registered_path(metadata),
        created: metadata.get_u64(CREATED).unwrap_or_default(),
    }
}

/// The instance id that the registration blob `metadata` holds, zero where it holds none or one
/// that isn't 16 bytes in hex.
fn registered_id(metadata: &Metadata) -> InstanceId {
    let mut id = InstanceId::default();
    match metadata.get_string(INSTANCE_ID) {
        Ok(text) if hex::decode_to_slice(text, id.as_mut_bytes()).is_ok() => id,
        _ => InstanceId::default(),
    }
}

/// The path that the registration blob `metadata` holds, empty where it holds none or one that
/// isn't UTF-8.
fn registered_path(metadata: &Metadata) -> String {
    metadata
        .get_string(PATH)
        .map(str::to_owned)
        .unwrap_or_default()
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
) -> Result<Vec<InstanceMetadata>, InstanceRegistryError> {
    use futures::StreamExt;

    let mut stream = repository
        .read_mutable_store()
        .list(repository.id, KeyType::Instance)
        .await
        .forward::<InstanceRegistryError>("failed to list instances")?;

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
        let branch =
            match load_anchor(repository, ANCHOR_CURRENT_BRANCH, instance.instance_id).await {
                Ok(Some(hash)) => hash.to_context(),
                _ => continue,
            };

        if branch == target_branch {
            matches.push(instance);
        }
    }
    Ok(matches)
}

/// The value of the anchor key of `instance_id` that `function` names: `None` where the key is
/// absent or holds zero.
async fn load_anchor(
    repository: &RepositoryContext,
    function: &str,
    instance_id: InstanceId,
) -> Result<Option<Hash>, StoreError> {
    let (key, key_type) = anchor_key(repository.salt(), function, instance_id);
    match repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
    {
        Ok(hash) => Ok(Some(hash).filter(|hash| !hash.is_zero())),
        Err(err) if err.is_absent() => Ok(None),
        Err(err) => Err(err),
    }
}

/// A registered instance on a branch, as [`records_by_branch`] and [`records_on_branch`] report
/// it.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceRecord {
    /// The instance's id, from its registration
    pub instance_id: InstanceId,
    /// The instance's working directory, as registered
    pub path: String,
    /// The instance's current revision, zero where it has none
    pub revision: Hash,
    /// Whether the registration still describes a live checkout
    pub staleness: InstanceStaleness,
}

/// The records of every registered instance with a current branch, grouped by that branch.
///
/// Lists the registrations once and reads each one's blob, from the local store alone, and its
/// current branch, then, for each instance with a current branch, its current revision and its
/// staleness ([`instance_staleness`]). Retires nothing. A failed store read fails the lookup rather
/// than leaving the instance out. A registration blob the store doesn't hold is skipped, and so is
/// one that names no usable instance id.
pub async fn records_by_branch(
    repository: &Arc<RepositoryContext>,
) -> Result<HashMap<BranchId, Vec<InstanceRecord>>, InstanceError> {
    let mut records: HashMap<BranchId, Vec<InstanceRecord>> = HashMap::new();
    visit_records(repository, None, |branch, record| {
        records.entry(branch).or_default().push(record);
    })
    .await?;
    Ok(records)
}

/// The records of the registered instances whose current branch is `branch`.
///
/// Reads as [`records_by_branch`] does, except that it reads the current revision and the
/// staleness only of the instances on `branch`.
pub async fn records_on_branch(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Vec<InstanceRecord>, InstanceError> {
    let mut records = Vec::new();
    visit_records(repository, Some(branch), |_, record| records.push(record)).await?;
    Ok(records)
}

/// Pass the current branch and record of each registered instance with a current branch to
/// `visit`, or only of those on `only`, reading as [`records_by_branch`] describes.
///
/// Every registration's blob is read, since only the blob holds the instance id the anchor keys
/// derive from. The instance of `repository` is visited as any other.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn visit_records<'a>(
    repository: &'a Arc<RepositoryContext>,
    only: Option<BranchId>,
    mut visit: impl FnMut(BranchId, InstanceRecord) + 'a,
) -> impl Future<Output = Result<(), InstanceError>> + 'a {
    async move {
        use futures::StreamExt;

        let mut registrations = repository
            .read_mutable_store()
            .list(repository.id, KeyType::Instance)
            .await
            .forward::<InstanceError>("listing registered instances")?;
        loop {
            let Some((_, metadata_hash)) = registrations.next().await else {
                return Ok(());
            };
            let Some((registration, instance_id)) =
                read_registration(repository, metadata_hash).await?
            else {
                continue;
            };
            if let Some((branch, record)) =
                instance_record(repository, only, registration, instance_id).await?
            {
                visit(branch, record);
            }
        }
    }
}

/// The current branch and record of the instance `instance_id` that the registration blob
/// `registration` holds, or `None` where it has no current branch or, where `only` is given,
/// another one. The path is read from the blob only for an instance this reports.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn instance_record(
    repository: &Arc<RepositoryContext>,
    only: Option<BranchId>,
    registration: Metadata,
    instance_id: InstanceId,
) -> impl Future<Output = Result<Option<(BranchId, InstanceRecord)>, InstanceError>> + '_ {
    async move {
        let Some(branch) = load_anchor(repository, ANCHOR_CURRENT_BRANCH, instance_id)
            .await
            .forward::<InstanceError>("loading the current branch of an instance")?
            .map(|hash| hash.to_context())
        else {
            return Ok(None);
        };
        if only.is_some_and(|only| only != branch) {
            return Ok(None);
        }
        let path = registered_path(&registration);
        let revision = load_anchor(repository, ANCHOR_CURRENT, instance_id)
            .await
            .forward::<InstanceError>("loading the current revision of an instance")?
            .unwrap_or_default();
        let staleness = instance_staleness(&path, instance_id).await;
        Ok(Some((
            branch,
            InstanceRecord {
                instance_id,
                path,
                revision,
                staleness,
            },
        )))
    }
}

/// The registration blob that `metadata_hash` names and the instance id it holds, read from the
/// local store alone with strict local reads, or `None` where `metadata_hash` is zero, as the key
/// of a retired registration holds, the store doesn't hold the blob, or the blob names no usable
/// instance id ([`registered_id`]). The anchors under the zero id are any that a context with a
/// zero instance id wrote, not that registration's.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn read_registration(
    repository: &Arc<RepositoryContext>,
    metadata_hash: Hash,
) -> impl Future<Output = Result<Option<(Metadata, InstanceId)>, InstanceError>> + '_ {
    async move {
        if metadata_hash.is_zero() {
            return Ok(None);
        }
        let options = ReadOptions::default().no_remote().with_strict_local();
        match Metadata::deserialize_with(repository.clone(), metadata_hash, options).await {
            Err(err) if err.is_address_not_found() => {
                lore_debug!(
                    "Skipping instance registration {metadata_hash}, whose blob the store doesn't \
                     hold"
                );
                Ok(None)
            }
            Ok(registration) => {
                let instance_id = registered_id(&registration);
                if instance_id.is_zero() {
                    lore_debug!(
                        "Skipping instance registration {metadata_hash}, whose blob names no \
                         usable instance id"
                    );
                    return Ok(None);
                }
                Ok(Some((registration, instance_id)))
            }
            Err(err) => Err(err).forward::<InstanceError>("reading an instance registration"),
        }
    }
}

/// Event data warning that several instances share the same checked-out branch.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
/// is zero, the repository has no revisions yet (fresh repo after create).
pub(crate) async fn load_current_anchor(
    repository: &Arc<RepositoryContext>,
) -> Result<(Hash, BranchId), AnchorError> {
    let revision = load_anchor(repository, ANCHOR_CURRENT, repository.instance_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let branch = load_anchor(repository, ANCHOR_CURRENT_BRANCH, repository.instance_id)
        .await
        .ok()
        .flatten()
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

/// Boxed version of [`load_current_anchor`] for cross-crate use.
pub fn load_current_anchor_boxed(
    repository: &Arc<RepositoryContext>,
) -> crate::BoxFuture<'_, Result<(Hash, BranchId), AnchorError>> {
    Box::pin(load_current_anchor(repository))
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
    if let Some(hash) = load_anchor(repository, ANCHOR_STAGED, repository.instance_id)
        .await
        .ok()
        .flatten()
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

/// Errors from writing and reading an instance registration: its metadata in the immutable store
/// and the key that names it in the mutable store.
#[error_set]
pub enum InstanceRegistryError {
    WriteRequired,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    Oversized,
    InvalidArguments,
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

#[error_set]
pub enum InstanceError {
    WriteRequired,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    Oversized,
    InvalidArguments,
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
    MountsBusy,
}

impl EventError for InstanceError {
    fn translated(&self) -> LoreError {
        match self {
            InstanceError::Disconnected(_) => LoreError::Connection,
            InstanceError::InvalidArguments(_) => LoreError::InvalidArguments,
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
///
/// Outside the service no mount manager resolves an SWFS instance, whose path
/// is absent or an empty mount point while it is not mounted. An SWFS instance
/// cannot be removed by hand, so its external `.lore` in the global data
/// directory is evidence it still exists, and outweighs a missing path or
/// checkout.
///
/// A FUSE instance's path is an empty mount point while it is not mounted, as
/// when the service is stopped, or mid-move, so its external `.lore`
/// outweighs a missing checkout too. A missing path still makes it stale: a
/// start removes the external directory of an instance whose mount point was
/// deleted.
///
/// An external `.lore` whose config is there but cannot be read or parsed is
/// no evidence the instance is gone, whichever VFS it was, so it keeps the
/// registration too.
pub async fn instance_staleness(path: &str, instance_id: InstanceId) -> InstanceStaleness {
    let staleness = path_staleness(path, instance_id).await;
    if !matches!(
        staleness,
        InstanceStaleness::PathMissing | InstanceStaleness::CheckoutMissing
    ) {
        return staleness;
    }
    let Ok(external) = crate::global::external_dir::external_lore_dir(instance_id) else {
        return staleness;
    };
    match external_vfs(&external).await {
        None | Some(VfsType::Swfs) => InstanceStaleness::Active,
        Some(VfsType::Fuse) if staleness == InstanceStaleness::CheckoutMissing => {
            InstanceStaleness::Active
        }
        Some(_) => staleness,
    }
}

/// Classify the registration of `instance_id` at `path` by what the path
/// holds, as [`instance_staleness`] does before consulting external `.lore`
/// directories.
async fn path_staleness(path: &str, instance_id: InstanceId) -> InstanceStaleness {
    if path.is_empty() {
        return InstanceStaleness::Active;
    }
    let io = lore_io::IoDriver::global();
    match io.stat(path).await {
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

/// The VFS the config in the external `.lore` directory at `dot_lore` names: [`VfsType::None`]
/// where it names none or there is no config, and nothing where a config is there but cannot be
/// read or parsed.
async fn external_vfs(dot_lore: &Path) -> Option<VfsType> {
    crate::util::config::load::<crate::repository::RepositoryConfig>(
        dot_lore.join(crate::repository::CONFIG),
    )
    .await
    .ok()
    .map(|config| config.vfs_type())
}

/// The branch (ID and name) and revision an instance has checked out, read
/// from its anchor keys. Zero or empty where an anchor is absent.
async fn instance_anchor_state(
    repository: &Arc<RepositoryContext>,
    instance_id: InstanceId,
) -> (BranchId, String, Hash) {
    let branch_id = load_anchor(repository, ANCHOR_CURRENT_BRANCH, instance_id)
        .await
        .ok()
        .flatten()
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

    let revision = load_anchor(repository, ANCHOR_CURRENT, instance_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    (branch_id, branch_name, revision)
}

use lore_storage::FlushOptions;

use crate::event::LoreEvent;

/// Event data describing an instance — used for both listing and prune notifications.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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

/// Event data for the start of an instance delete operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceDeleteBeginEventData {
    /// Identifier of the instance being deleted
    pub instance_id: InstanceId,
    /// Filesystem path of the instance
    pub path: LoreString,
    /// Whether this is a SWFS-backed instance
    pub is_swfs: u8,
}

/// Phase of an instance delete operation.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub enum LoreRepositoryInstanceDeletePhase {
    /// Unmounting the instance, an SWFS instance or a virtual clone
    Unmounting = 0,
    /// Removing instance keys from mutable store
    Deregistering = 1,
    /// Deleting external directories or repository directory
    DeletingFiles = 2,
}

/// Event data for progress during an instance delete operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceDeleteProgressEventData {
    /// Current phase of the delete operation
    pub phase: LoreRepositoryInstanceDeletePhase,
}

/// Event data for the completion of an instance delete operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceDeleteCompleteEventData {
    /// Identifier of the deleted instance
    pub instance_id: InstanceId,
}

/// Event data for the start of an instance move operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceMoveBeginEventData {
    /// Identifier of the instance being moved
    pub instance_id: InstanceId,
    /// Original filesystem path of the instance
    pub source_path: LoreString,
    /// New filesystem path for the instance
    pub target_path: LoreString,
}

/// Event data for the completion of an instance move operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceMoveCompleteEventData {
    /// Identifier of the moved instance
    pub instance_id: InstanceId,
    /// New filesystem path of the instance
    pub target_path: LoreString,
}

/// How an instance clone is made, as decided before the clone writes anything.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub enum LoreRepositoryInstanceCloneMethod {
    /// The clone shares the instance's data blocks, costing filesystem metadata, not content
    SharesBlocks = 0,
    /// The clone copies the instance's files
    CopiesBytes = 1,
    /// The instance cannot be cloned to the destination
    Unsupported = 2,
}

/// Event data for the start of an instance clone operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceCloneBeginEventData {
    /// Identifier of the instance being cloned
    pub instance_id: InstanceId,
    /// Filesystem path of the instance being cloned
    pub source_path: LoreString,
    /// Filesystem path the clone is created at
    pub target_path: LoreString,
    /// How the clone is made
    pub method: LoreRepositoryInstanceCloneMethod,
    /// Filesystem of the volume the clone is created on; empty for
    /// `LORE_REPOSITORY_INSTANCE_CLONE_METHOD_UNSUPPORTED`
    pub filesystem: LoreString,
    /// Mount point of the volume the clone is created on; empty for
    /// `LORE_REPOSITORY_INSTANCE_CLONE_METHOD_UNSUPPORTED`
    pub mount_point: LoreString,
    /// Bytes the copy takes on that volume, for `LORE_REPOSITORY_INSTANCE_CLONE_METHOD_COPIES_BYTES`;
    /// zero otherwise
    pub size: u64,
    /// Number of files the copy writes, for `LORE_REPOSITORY_INSTANCE_CLONE_METHOD_COPIES_BYTES`;
    /// zero otherwise
    pub file_count: u64,
    /// Bytes free on that volume, for `LORE_REPOSITORY_INSTANCE_CLONE_METHOD_COPIES_BYTES`; zero
    /// otherwise
    pub available_size: u64,
    /// Why the clone copies or cannot be made; empty for
    /// `LORE_REPOSITORY_INSTANCE_CLONE_METHOD_SHARES_BLOCKS`
    pub reason: LoreString,
}

/// Phase of an instance clone operation.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub enum LoreRepositoryInstanceClonePhase {
    /// Removing a directory an interrupted clone to the same destination left, named by the
    /// event's path, before the instance's lock is taken
    RemovingLeftover = 0,
    /// Writing the instance's modified-time records back to disk
    WritingRecords = 1,
    /// Copying the instance's directory, sent again each event interval with the counts so far
    Copying = 2,
    /// Giving the clone the instance's anchors, moving it to its path and registering it
    Registering = 3,
}

/// Event data for progress during an instance clone operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceCloneProgressEventData {
    /// Current phase of the clone operation
    pub phase: LoreRepositoryInstanceClonePhase,
    /// The directory the phase removes, for `LORE_REPOSITORY_INSTANCE_CLONE_PHASE_REMOVING_LEFTOVER`;
    /// empty otherwise
    pub path: LoreString,
    /// Directories cloned as a whole so far, each with everything in it
    pub cloned_directory_count: u64,
    /// Files and symbolic links cloned one by one so far
    pub cloned_entry_count: u64,
    /// Files copied so far
    pub copied_file_count: u64,
}

/// Event data for the completion of an instance clone operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryInstanceCloneCompleteEventData {
    /// Identifier of the new instance, zero in a dry run
    pub instance_id: InstanceId,
    /// Filesystem path of the new instance
    pub target_path: LoreString,
    /// Directories cloned as a whole, each with everything in it
    pub cloned_directory_count: u64,
    /// Files and symbolic links cloned one by one
    pub cloned_entry_count: u64,
    /// Files whose data was copied
    pub copied_file_count: u64,
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
/// path holds no checkout at all. An SWFS instance with an external `.lore` is
/// kept whether or not it is mounted; see [`instance_staleness`].
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
    register_instance(&repository, repository.instance_id, &current_path)
        .await
        .forward::<InstanceError>("registering the instance at its current path")?;
    lore_debug!("Updated instance path to {current_path}");
    Ok(())
}

/// Whether the instance's stores are shared, and so outside its directory.
///
/// Reads the instance's config and resolves the shared store it names, so a missing or
/// mismatched shared store fails here rather than part way through the operation. Opens no
/// store, takes no lock and makes no connection.
async fn uses_shared_store(repository: &Arc<RepositoryContext>) -> Result<bool, InstanceError> {
    let dot_path = repository.dot_dir_path().map_err(|e| {
        InstanceError::internal(format!("failed to find the instance directory: {e}"))
    })?;
    let config = crate::repository::load_repository_config_from_dot_dir(dot_path)
        .forward_any::<InstanceError>("failed to load the repository config")?;
    Ok(crate::shared_store::get_shared_store_path_for_repo(&config)
        .await
        .forward_any::<InstanceError>("failed to find the shared store")?
        .is_some())
}

/// Whether the instance at `path` can be acted on, given whether it `is_mounted` by the service,
/// whether a mount manager is `in_service`, and the instance the mount manager has `mounted` at
/// `path`.
///
/// An instance counts as mounted, by SWFS or FUSE, when an external directory exists for the
/// instance id in its `.lore/instance` file, but a directory can name any id. Acting on that id
/// alone would let a checkout that names another instance move or delete that instance's files,
/// while the mount at `path` stayed with the instance it belongs to. A mount of `instance_id`
/// itself at `path` is what ties the path to the id, so a mounted instance needs one, and any
/// other instance must not be on a mount at all.
#[lore_macro::test_pub]
fn check_mount(
    instance_id: InstanceId,
    is_mounted: bool,
    in_service: bool,
    mounted: Option<InstanceId>,
    path: &str,
    action: &str,
) -> Result<(), InstanceError> {
    match mounted {
        Some(mounted) if mounted != instance_id => Err(InstanceError::internal(format!(
            "{path} is the mount of instance {mounted}, but its instance file names instance \
             {instance_id}, so it cannot be told which to {action}"
        ))),
        Some(_) if !is_mounted => Err(InstanceError::internal(format!(
            "{path} is the mount of instance {instance_id}, which has no external directory, so \
             it cannot be told how to {action} it"
        ))),
        Some(_) => Ok(()),
        None if !is_mounted => Ok(()),
        None if !in_service => Err(InstanceError::internal(format!(
            "Instance {instance_id} is a VFS instance, which only the service can {action}"
        ))),
        None => Err(InstanceError::internal(format!(
            "Instance {instance_id} is not mounted at {path}, so it is not the VFS instance \
             that directory's instance id names"
        ))),
    }
}

/// The VFS serving the instance at `path`, after checking as [`check_mount`] does that it can be
/// acted on: the VFS of the service's mount of `instance_id` at `path`, which the check requires
/// of an instance with an external directory, or [`VfsType::None`] for one in a directory of its
/// own.
fn require_own_mount(
    instance_id: InstanceId,
    path: &str,
    action: &str,
) -> Result<VfsType, InstanceError> {
    let mount_manager = MountManagerState::mount_manager();
    let mounted = match &mount_manager {
        Some(mount_manager) => mount_manager
            .mounted(Path::new(path))
            .forward_any::<InstanceError>("failed to look up the mount")?,
        None => None,
    };
    check_mount(
        instance_id,
        matches!(check_for_external_lore_dir(instance_id), Ok(Some(_))),
        mount_manager.is_some(),
        mounted.map(|(mounted, _)| mounted),
        path,
        action,
    )?;
    Ok(mounted.map_or(VfsType::None, |(_, vfs)| vfs))
}

/// Fail where Windows would refuse to rename or delete `instance_path` because a process has it
/// as its working directory, before anything has been changed.
fn require_working_directory_outside(
    instance_path: &str,
    action: &str,
) -> Result<(), InstanceError> {
    if !cfg!(target_family = "windows") {
        return Ok(());
    }
    // The working directory the call carries, which for a call relayed to a service is the
    // caller's rather than the service's, and this process's own only when the call names none.
    let Ok(working_directory) = crate::util::path::make_absolute(".") else {
        return Ok(());
    };
    let working_directory =
        crate::util::path::clean(working_directory.to_string_lossy().into_owned()).to_lowercase();
    let instance = crate::util::path::clean(instance_path.to_owned()).to_lowercase();
    if working_directory == instance || working_directory.starts_with(&format!("{instance}/")) {
        return Err(InstanceError::internal(format!(
            "Cannot {action} the instance at {instance_path} while it is the working directory. \
             Run the command from outside the instance, naming it with --repository"
        )));
    }
    Ok(())
}

/// First wait before a move or delete reissues a file operation that failed because something
/// had the instance's files in use, in milliseconds.
#[lore_macro::test_pub]
const IN_USE_RETRY_START_MILLIS: u64 = 50;

/// Longest wait the backoff grows to, in milliseconds.
#[lore_macro::test_pub]
const IN_USE_RETRY_MAXIMUM_MILLIS: u64 = 2_000;

/// Waits before an operation that keeps failing in use is reported. With the schedule above this
/// waits about fifteen seconds, longer than the ten seconds a finished command in the same process
/// keeps the instance's stores open by default.
#[lore_macro::test_pub]
const IN_USE_RETRY_LIMIT: usize = 12;

/// Whether `error` says that something has the files in use, so the operation can succeed once
/// it lets go: another Lore command reading the instance, in this process or another, or a
/// program the user has open in it.
#[lore_macro::test_pub]
fn is_in_use(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ResourceBusy || is_in_use_for_platform(error)
}

/// Whether a failed rename of an instance to `to` is worth reissuing: the files were in use and
/// the target is still free. Windows reports a rename onto a path that is taken as access denied,
/// the same error a file open in the directory gives, and no wait frees that path.
#[lore_macro::test_pub]
fn rename_is_retryable<'a>(
    error: &io::Error,
    to: &'a Path,
) -> impl Future<Output = bool> + use<'a> {
    let in_use = is_in_use(error);
    async move { in_use && is_free(to).await }
}

/// Whether nothing is at `path`, linked or not. A path that cannot be checked counts as taken.
async fn is_free(path: &Path) -> bool {
    matches!(
        lore_io::IoDriver::global().symlink_stat(path).await,
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

/// Whether a failed delete of an instance directory is worth reissuing: the files were in use, or
/// the directory was left non-empty, as a delete that races a reader creating a file leaves it,
/// and on Windows a delete of a file whose removal waits on a handle still open.
#[lore_macro::test_pub]
fn delete_is_retryable(error: &io::Error) -> bool {
    is_in_use(error) || error.kind() == io::ErrorKind::DirectoryNotEmpty
}

/// Windows refuses to rename a directory with a file open in it as access denied, and to delete
/// a file another handle shares no deletion with as a sharing violation.
#[cfg(target_family = "windows")]
fn is_in_use_for_platform(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32
                || code == windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32
                || code == windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32
    )
}

/// No other kernel refuses a rename or delete because a file is open.
#[cfg(not(target_family = "windows"))]
fn is_in_use_for_platform(_error: &io::Error) -> bool {
    false
}

/// Runs `operation`, reissuing it with backoff for as long as it fails with an error `retryable`
/// accepts, up to [`IN_USE_RETRY_LIMIT`] times. Any other failure is reported at once.
///
/// Takes plain closures returning futures rather than `AsyncFn`s, whose higher-ranked lifetimes
/// defeat `Send` inference for the callers that spawn these commands.
#[lore_macro::test_pub]
async fn retry_while_in_use<T, E, F, O, R, RO>(
    action: &str,
    retryable: R,
    mut operation: F,
) -> Result<T, E>
where
    E: fmt::Display,
    F: FnMut() -> O,
    O: Future<Output = Result<T, E>>,
    R: Fn(&E) -> RO,
    RO: Future<Output = bool>,
{
    let mut retry = crate::util::time::retry(
        IN_USE_RETRY_START_MILLIS,
        IN_USE_RETRY_MAXIMUM_MILLIS,
        IN_USE_RETRY_LIMIT,
    );
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if retryable(&error).await && retry.wait().await => {
                lore_debug!("Retrying to {action}, which failed in use: {error}");
            }
            Err(error) => return Err(error),
        }
    }
}

/// A delete of an instance's files that failed, with the directory that holds what is left.
#[lore_macro::test_pub]
#[derive(Debug)]
struct FailedRemoval {
    directory: PathBuf,
    error: io::Error,
}

impl fmt::Display for FailedRemoval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to delete {}: {}",
            self.directory.display(),
            self.error
        )
    }
}

impl std::error::Error for FailedRemoval {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A [`FailedRemoval`] that ended a delete, with what the delete had done to the instance
/// before it, which the message tells the user along with the directory to delete by hand.
#[derive(Debug)]
struct IncompleteDelete {
    failure: FailedRemoval,
    deregistered: bool,
    unmounted: bool,
}

impl fmt::Display for IncompleteDelete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let done = match (self.deregistered, self.unmounted) {
            (true, true) => "deregistered and unmounted",
            (true, false) => "deregistered",
            (false, true) => "unmounted",
            (false, false) => return write!(f, "{}. Delete that directory manually", self.failure),
        };
        write!(
            f,
            "{}. The instance is already {done}; delete that directory manually",
            self.failure
        )
    }
}

impl std::error::Error for IncompleteDelete {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.failure)
    }
}

/// Delete the directory at `path` and everything in it, renamed aside to `aside` first where one
/// is given, as [`lore_io::IoDriver::remove_dir_all_in_background`] does, retrying while its
/// files are in use, and with `make_writable` making the directories of a tree that refuses the
/// removal writable by their owner, as a copy keeps the permissions of a read-only directory.
async fn remove_instance_directory(
    path: &Path,
    aside: Option<&Path>,
    make_writable: bool,
) -> Result<(), FailedRemoval> {
    remove_instance_directory_with(path, aside, |path, aside| {
        lore_io::IoDriver::global().remove_dir_all_in_background(path, aside, make_writable)
    })
    .await
}

/// [`remove_instance_directory`] with each attempt made by `attempt`, which takes the arguments
/// of [`lore_io::IoDriver::remove_dir_all_in_background`] and answers as it does.
///
/// Once an attempt answers `aside`, it renamed the directory there, and every later attempt
/// removes `aside` alone: `path` is free from then on, and whatever is made there is not the
/// instance's.
#[lore_macro::test_pub]
async fn remove_instance_directory_with<A, AO>(
    path: &Path,
    aside: Option<&Path>,
    attempt: A,
) -> Result<(), FailedRemoval>
where
    A: Fn(PathBuf, Option<PathBuf>) -> AO,
    AO: Future<Output = (PathBuf, io::Result<()>)>,
{
    let action = format!("delete {}", path.display());
    let renamed = AtomicBool::new(false);
    retry_while_in_use(
        &action,
        |failure| removal_is_retryable(failure, aside),
        || {
            let attempted = match aside {
                Some(aside) if renamed.load(Ordering::Relaxed) => {
                    attempt(aside.to_path_buf(), None)
                }
                aside => attempt(path.to_path_buf(), aside.map(Path::to_path_buf)),
            };
            let renamed = &renamed;
            async move {
                match attempted.await {
                    (_, Ok(())) => Ok(()),
                    (directory, Err(error)) => {
                        if Some(directory.as_path()) == aside {
                            renamed.store(true, Ordering::Relaxed);
                        }
                        Err(FailedRemoval { directory, error })
                    }
                }
            }
        },
    )
    .await
}

/// Whether a failed delete of an instance directory is worth reissuing: the delete is, as
/// [`delete_is_retryable`] answers, and where a rename aside to `aside` is what failed, the aside
/// name is still free.
#[lore_macro::test_pub]
fn removal_is_retryable<'a>(
    failure: &FailedRemoval,
    aside: Option<&'a Path>,
) -> impl Future<Output = bool> + use<'a> {
    let retryable = delete_is_retryable(&failure.error);
    let unrenamed = aside.filter(|aside| failure.directory != *aside);
    async move {
        retryable
            && match unrenamed {
                Some(aside) => is_free(aside).await,
                None => true,
            }
    }
}

/// The sibling of the directory at `path` that a delete renames it to before removing its files:
/// `<name>.deleting-<instance ID>`.
#[lore_macro::test_pub]
fn deleting_aside_path(path: &Path, instance_id: InstanceId) -> PathBuf {
    sibling_with_suffix(path, &format!(".deleting-{instance_id}"))
}

/// The sibling of `path` whose name is `path`'s name followed by `suffix`.
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(suffix);
    path.with_file_name(name)
}

/// Fail if something already holds `aside`, the name a delete renames the instance to, so a
/// delete never removes a directory it did not put there.
async fn require_free_aside_path(aside: &Path) -> Result<(), InstanceError> {
    match lore_io::IoDriver::global().symlink_stat(aside).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(InstanceError::internal(format!(
            "{} already exists, so the instance cannot be renamed there to delete it. Remove it \
             first",
            aside.display()
        ))),
        Err(error) => Err(InstanceError::internal_with_context(
            error,
            &format!("checking that {} is free", aside.display()),
        )),
    }
}

/// Remove the registration of the instance at `path`: open it as a repository, as a command run
/// from there would, and remove its keys from the registry.
async fn deregister_instance_at(path: &Path, instance_id: InstanceId) -> Result<(), InstanceError> {
    deregister_instance(open_instance_at(path).await?, instance_id).await
}

/// Open the instance at `path` as a repository, as a command run from there would, to remove its
/// registration with [`deregister_instance`].
async fn open_instance_at(path: &Path) -> Result<Arc<RepositoryContext>, InstanceError> {
    let token = crate::repository::RepositoryWriteToken::acquire(path).await;
    crate::repository::load_and_connect_with_token(
        path,
        crate::repository::RepositoryAccess::ReadWrite,
        Some(token),
    )
    .await
    .forward_any::<InstanceError>("failed to open the instance")
}

/// Remove the registration of `instance_id` from `repository`, the instance opened by
/// [`open_instance_at`], and close it again.
async fn deregister_instance(
    repository: Arc<RepositoryContext>,
    instance_id: InstanceId,
) -> Result<(), InstanceError> {
    if !remove_instance_keys(&repository, instance_id).await {
        return Err(WriteRequired.into());
    }
    repository
        .flush(FlushOptions::default())
        .await
        .forward_any::<InstanceError>("failed to flush the instance removal")
}

/// Delete the current instance: remove its registration, unmount it (an SWFS or FUSE instance, in
/// the service), and delete its directories.
///
/// A FUSE instance is unmounted before anything changes, with a plain unmount a busy mount
/// refuses, so a mount in use leaves the instance as it was. Otherwise the registration goes
/// first. Either way the repository is closed again before anything is deleted, as Windows cannot
/// delete files that are open. Nothing is destroyed if the instance cannot be opened to
/// deregister it. The directory with the instance's files, the instance directory or a mounted
/// instance's external directory, is then renamed aside to [`deleting_aside_path`], so its path
/// is free before its files are deleted, unless the directory is a mount point, or on Windows a
/// volume mounted in a folder or a junction, which is deleted in place, as
/// [`lore_io::IoDriver::remove_dir_all_in_background`] describes. A delete that fails after the
/// deregistration leaves files behind, and says where, for the user to remove.
///
/// In a dry run (the global `dry_run` flag), checks everything a delete would need and emits
/// events describing what would be deleted, without performing any changes.
///
/// The caller opens the repository without its stores or directory lock, so that this function
/// controls when they are held.
pub async fn instance_delete(repository: Arc<RepositoryContext>) -> Result<(), InstanceError> {
    instance_delete_with(repository, deregister_instance_at).await
}

/// [`instance_delete`] with the step that deregisters the instance supplied by the caller, so a
/// failed deregistration can be tested.
///
/// `deregister` removes the registration of the instance at a path, as
/// [`deregister_instance_at`] does. A FUSE instance, opened before it is unmounted, is
/// deregistered through that opened repository instead.
#[lore_macro::test_pub]
async fn instance_delete_with(
    repository: Arc<RepositoryContext>,
    deregister: impl AsyncFn(&Path, InstanceId) -> Result<(), InstanceError>,
) -> Result<(), InstanceError> {
    let dry_run = crate::lore::execution_context().globals().dry_run();
    let instance_id = repository.instance_id;
    let path = repository.path_for_display().to_string();
    // Everything that can fail on a bad configuration is checked before anything is changed, and
    // by a dry run too.
    let vfs = require_own_mount(instance_id, &path, "delete")?;

    // Emit begin event
    LoreEvent::RepositoryInstanceDeleteBegin(LoreRepositoryInstanceDeleteBeginEventData {
        instance_id,
        path: LoreString::from_str(&path),
        is_swfs: u8::from(vfs == VfsType::Swfs),
    })
    .send();

    require_working_directory_outside(&path, "delete")?;
    let shared_store = uses_shared_store(&repository).await?;
    // The directory a delete removes, and the name it renames that directory to first. A taken
    // name is refused in every case, as only a refused rename tells a mount point.
    let directory = if vfs.is_virtual() {
        external_dir_for_instance(instance_id)
            .forward_any::<InstanceError>("failed to find the external directory")?
    } else {
        PathBuf::from(&path)
    };
    let aside = deleting_aside_path(&directory, instance_id);
    require_free_aside_path(&aside).await?;

    // A FUSE instance is unmounted first, as a busy mount refuses it and so leaves the instance
    // whole; an SWFS one after it is deregistered.
    let phases: &[LoreRepositoryInstanceDeletePhase] = match vfs {
        VfsType::None => &[
            LoreRepositoryInstanceDeletePhase::Deregistering,
            LoreRepositoryInstanceDeletePhase::DeletingFiles,
        ],
        VfsType::Swfs => &[
            LoreRepositoryInstanceDeletePhase::Deregistering,
            LoreRepositoryInstanceDeletePhase::Unmounting,
            LoreRepositoryInstanceDeletePhase::DeletingFiles,
        ],
        VfsType::Fuse => &[
            LoreRepositoryInstanceDeletePhase::Unmounting,
            LoreRepositoryInstanceDeletePhase::Deregistering,
            LoreRepositoryInstanceDeletePhase::DeletingFiles,
        ],
    };

    if dry_run {
        // Dry run: report what would happen
        for &phase in phases {
            LoreEvent::RepositoryInstanceDeleteProgress(
                LoreRepositoryInstanceDeleteProgressEventData { phase },
            )
            .send();
        }
        LoreEvent::RepositoryInstanceDeleteComplete(
            LoreRepositoryInstanceDeleteCompleteEventData { instance_id },
        )
        .send();
        return Ok(());
    }

    // Only the mount leads a command at `path` to the instance's external `.lore`, so a FUSE
    // instance is opened before it is unmounted. That opens nothing through the mount, which the
    // unmount would then find busy.
    let mut opened = None;
    if shared_store && vfs == VfsType::Fuse {
        opened = Some(open_instance_at(Path::new(&path)).await?);
    }
    // The opened instance holds the write token until it is deregistered; one with nothing to
    // deregister holds it alone. No command writes to the instance while it is unmounted, and
    // once it is, none can open it at its path.
    let _token = if !shared_store && vfs == VfsType::Fuse {
        Some(crate::repository::RepositoryWriteToken::acquire(Path::new(&path)).await)
    } else {
        None
    };
    for &phase in phases {
        LoreEvent::RepositoryInstanceDeleteProgress(
            LoreRepositoryInstanceDeleteProgressEventData { phase },
        )
        .send();
        match phase {
            LoreRepositoryInstanceDeletePhase::Deregistering => {
                // A registry inside the instance directory goes with it.
                if shared_store {
                    let deregistered = match opened.take() {
                        Some(repository) => deregister_instance(repository, instance_id).await,
                        None => deregister(Path::new(&path), instance_id).await,
                    };
                    if vfs == VfsType::Fuse {
                        // Unmounted by now, and still registered, so a start mounts it again.
                        deregistered.forward_with::<InstanceError, _>(|| {
                            format!(
                                "deregistering the unmounted instance, which the service mounts \
                                 at {path} again when it restarts"
                            )
                        })?;
                    } else {
                        deregistered?;
                    }
                }
            }
            LoreRepositoryInstanceDeletePhase::Unmounting => {
                if vfs == VfsType::Fuse {
                    unmount_fuse_instance(&path).await?;
                } else if let Some(mm) = MountManagerState::mount_manager() {
                    mm.swfs()
                        .remove_mount(Path::new(&path))
                        .forward::<InstanceError>("unmounting the instance")?;
                }
            }
            LoreRepositoryInstanceDeletePhase::DeletingFiles => {
                delete_instance_files(vfs, shared_store, &path, &directory, &aside).await?;
            }
        }
    }

    LoreEvent::RepositoryInstanceDeleteComplete(LoreRepositoryInstanceDeleteCompleteEventData {
        instance_id,
    })
    .send();
    lore_debug!("Deleted instance {instance_id} at {path}");
    Ok(())
}

/// Remove what a delete leaves of an instance on disk: `directory`, the instance directory or a
/// mounted instance's external directory, renamed aside to `aside` first, and for FUSE the mount
/// point at `path` the unmount emptied. `deregistered` says whether the delete removed the
/// instance's registration, which a failure tells the user.
async fn delete_instance_files(
    vfs: VfsType,
    deregistered: bool,
    path: &str,
    directory: &Path,
    aside: &Path,
) -> Result<(), InstanceError> {
    remove_instance_directory(directory, Some(aside), false)
        .await
        .map_err(|failure| {
            let incomplete = IncompleteDelete {
                failure,
                deregistered,
                unmounted: vfs.is_virtual(),
            };
            InstanceError::internal_with_context(incomplete, "deleting the instance's files")
        })?;
    // Not removed recursively: it is empty once unmounted, and a mount still standing there
    // would take whatever it serves with it.
    if vfs == VfsType::Fuse
        && let Err(e) = lore_io::IoDriver::global().remove_dir(path).await
        && !is_absent(&e)
    {
        return Err(InstanceError::internal(format!(
            "Failed to remove the mount point {path}: {e}. The instance is already deleted; \
             remove that directory manually"
        )));
    }
    Ok(())
}

/// Unmount the FUSE instance at `path`, a plain unmount that a busy mount refuses.
#[cfg(target_os = "linux")]
async fn unmount_fuse_instance(path: &str) -> Result<(), InstanceError> {
    let Some(mm) = MountManagerState::mount_manager() else {
        return Err(InstanceError::internal(format!(
            "The instance at {path} is a VFS instance, which only the service can unmount"
        )));
    };
    // The mount manager's error says why, `MountsBusy` naming the busy path.
    mm.fuse()
        .unmount(Path::new(path), false)
        .await
        .forward_with::<InstanceError, _>(|| format!("unmounting the instance at {path}"))
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
async fn unmount_fuse_instance(path: &str) -> Result<(), InstanceError> {
    Err(InstanceError::internal(format!(
        "The instance at {path} is a virtual clone, which only Linux serves"
    )))
}

/// Record where the instance at `path` now is: open it as a repository, as a command run from
/// there would, and update its registered path as `lore repository update-path` does.
async fn register_instance_at(path: &Path) -> Result<(), InstanceError> {
    let token = crate::repository::RepositoryWriteToken::acquire(path).await;
    let repository = crate::repository::load_and_connect_with_token(
        path,
        crate::repository::RepositoryAccess::ReadWrite,
        Some(token),
    )
    .await
    .forward_any::<InstanceError>("failed to open the instance")?;
    update_path(repository.clone()).await?;
    // Synced, since a move then removes the mount point the old registration names.
    repository
        .flush(FlushOptions::SYNC_DATA)
        .await
        .forward_any::<InstanceError>("failed to flush the instance registration")
}

/// Move an instance's directory, or its mount for an SWFS or FUSE instance in a service.
async fn move_instance(
    vfs: VfsType,
    instance_id: InstanceId,
    from: &str,
    to: &str,
) -> Result<(), InstanceError> {
    if !vfs.is_virtual() {
        let action = format!("move {from} to {to}");
        let retryable = |error: &io::Error| rename_is_retryable(error, Path::new(to));
        return retry_while_in_use(&action, retryable, || {
            lore_io::IoDriver::global().rename(from, to)
        })
        .await
        .map_err(|e| {
            InstanceError::internal(format!(
                "Failed to move directory from {from} to {to}: {e}. Move the directory \
                     manually, then run `lore repository update-path` in it at its new location"
            ))
        });
    }
    let Some(mm) = MountManagerState::mount_manager() else {
        return Err(InstanceError::internal(format!(
            "Instance {instance_id} is a VFS instance, which only the service can move"
        )));
    };
    if vfs == VfsType::Fuse {
        return move_fuse_mount(&mm, instance_id, from, to).await;
    }
    mm.swfs()
        .remount(Path::new(from), Path::new(to))
        .await
        .map_err(|e| InstanceError::internal(format!("Failed to remount instance at {to}: {e}")))
}

/// Move the FUSE mount of `instance_id` from `from` to `to`, holding the instance's write token
/// at `from` throughout, so no command writing to the instance there, such as a commit or a sync,
/// runs while it is unmounted and mounted again, and no other move or delete of it does.
#[cfg(target_os = "linux")]
async fn move_fuse_mount(
    mm: &crate::fs::mount_manager::MountManager,
    instance_id: InstanceId,
    from: &str,
    to: &str,
) -> Result<(), InstanceError> {
    let _token = crate::repository::RepositoryWriteToken::acquire(Path::new(from)).await;
    mm.fuse()
        .move_mount(instance_id, Path::new(from), Path::new(to))
        .await
        .forward_with::<InstanceError, _>(|| format!("moving the mount to {to}"))
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
async fn move_fuse_mount(
    _mm: &crate::fs::mount_manager::MountManager,
    _instance_id: InstanceId,
    from: &str,
    _to: &str,
) -> Result<(), InstanceError> {
    Err(InstanceError::internal(format!(
        "The instance at {from} is a virtual clone, which only Linux serves"
    )))
}

/// Remove the mount point a FUSE move left at `path` once the instance's registration no longer
/// names it. Until then it stays, so a service starting meanwhile finds the instance's registered
/// path and mounts it there. One left behind is only an empty directory, so a failure is reported
/// and the move stands.
async fn remove_left_mount_point(vfs: VfsType, path: &str) {
    if vfs != VfsType::Fuse {
        return;
    }
    if let Err(error) = lore_io::IoDriver::global().remove_dir(path).await
        && !is_absent(&error)
    {
        lore_warn!("Unable to remove {path}, the mount point the instance moved from: {error}");
    }
}

/// A move or a clone refused for its target path.
fn refused_target(reason: String) -> InstanceError {
    InvalidArguments { reason }.into()
}

/// The directories above `target` that don't exist, nearest first, and the nearest one that does,
/// `None` where none does. An ancestor that exists in any form, a symbolic link or a file
/// included, ends the walk, and one that can't be checked fails it. A path below a file doesn't
/// exist, so the walk goes on to the file.
fn missing_parents(target: &Path) -> Result<(Vec<PathBuf>, Option<&Path>), InstanceError> {
    let mut missing = Vec::new();
    for directory in target.ancestors().skip(1) {
        if directory.as_os_str().is_empty() {
            return Ok((missing, Some(directory)));
        }
        match directory.symlink_metadata() {
            Ok(_) => return Ok((missing, Some(directory))),
            Err(error) if is_absent(&error) || error.kind() == io::ErrorKind::NotADirectory => {
                missing.push(directory.to_path_buf());
            }
            Err(error) => {
                return Err(InstanceError::internal(format!(
                    "Unable to check {}, above the target path {}: {error}",
                    directory.display(),
                    target.display()
                )));
            }
        }
    }
    Ok((missing, None))
}

/// Whether `existing`, the nearest directory above a move's target that exists, leads into the
/// instance at `source` through a symbolic link, which the cleaned paths that
/// [`is_path_inside_repository`](crate::util::path::is_path_inside_repository) compares can't
/// show. Only the directory above the instance is resolved, as a move renames the instance's own
/// entry rather than what that entry may link to.
fn leads_into_instance(existing: &Path, source: &Path) -> Result<bool, InstanceError> {
    let resolve = |path: &Path| {
        let path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        path.canonicalize().map_err(|error| {
            InstanceError::internal(format!("Unable to resolve {}: {error}", path.display()))
        })
    };
    let instance = match (source.parent(), source.file_name()) {
        (Some(parent), Some(name)) => resolve(parent)?.join(name),
        _ => resolve(source)?,
    };
    Ok(resolve(existing)?.starts_with(instance))
}

/// Create `missing`, the directories above `target_path` that [`missing_parents`] found, so the
/// instance can move there. A failure removes what it created.
async fn create_parents(missing: &[PathBuf], target_path: &str) -> Result<(), InstanceError> {
    let Some(nearest) = missing.first() else {
        return Ok(());
    };
    if let Err(error) = lore_io::IoDriver::global().create_dir_all(nearest).await {
        remove_created_parents(missing).await;
        return Err(InstanceError::internal(format!(
            "Failed to create {}, the directory to move the instance into at {target_path}: \
             {error}",
            nearest.display()
        )));
    }
    Ok(())
}

/// Remove the directories [`create_parents`] made for a move that did not stand, nearest first,
/// stopping at one that can't be removed, such as one something else has written into since. One
/// left behind is reported, and the outcome of the move stands.
async fn remove_created_parents(created: &[PathBuf]) {
    for directory in created {
        match lore_io::IoDriver::global().remove_dir(directory).await {
            Ok(()) => {}
            Err(error) if is_absent(&error) => {}
            Err(error) => {
                lore_warn!(
                    "Unable to remove {}, which the move created: {error}",
                    directory.display()
                );
                return;
            }
        }
    }
}

/// Move the current instance to a new path: create the directories above it that do not exist,
/// move its directory (or its mount for an SWFS or FUSE instance in a service), and then record the
/// new path.
///
/// The registration is written last so a move that fails leaves the instance registered where it
/// is, and a registration that cannot be written undoes the move. A move that does not stand
/// removes the directories it created. A FUSE instance's old mount point goes only after that, as
/// a service that starts mounts the instance at its registered path.
///
/// The target is cleaned first, so a target that leaves the instance through `..` names the
/// directory it reaches. A target that exists (a symbolic link that leads nowhere included), is
/// inside the instance (through a symbolic link too), is below a file, has no existing directory
/// above it, is an instance mount, or is nested in another repository or a virtual instance's
/// mount is refused with [`InstanceError::InvalidArguments`].
///
/// In a dry run (the global `dry_run` flag), checks everything a move would need and reports it
/// without moving or registering anything.
///
/// The caller opens the repository without its stores or directory lock. Windows cannot rename a
/// directory that has files open in it, and the stores and lock are files in the instance
/// directory.
pub async fn instance_move(
    repository: Arc<RepositoryContext>,
    target_path: &str,
) -> Result<(), InstanceError> {
    instance_move_with(repository, target_path, register_instance_at).await
}

/// [`instance_move`] with the step that records an instance's path supplied by the caller, so a
/// failure of that step, and the rollback it triggers, can be tested.
///
/// `register` records the instance at the path it is given, as [`register_instance_at`] does.
#[lore_macro::test_pub]
async fn instance_move_with(
    repository: Arc<RepositoryContext>,
    target_path: &str,
    register: impl AsyncFn(&Path) -> Result<(), InstanceError>,
) -> Result<(), InstanceError> {
    let dry_run = crate::lore::execution_context().globals().dry_run();
    let instance_id = repository.instance_id;
    let source_path = repository.path_for_display().to_string();
    let target_path = &crate::util::path::clean(target_path.to_owned());
    let target = Path::new(target_path);

    // Validate target path. A symbolic link there exists even where it leads nowhere, and a
    // rename would replace it.
    if target.symlink_metadata().is_ok() {
        return Err(refused_target(format!(
            "Target path already exists: {target_path}"
        )));
    }

    if crate::util::path::is_path_inside_repository(Path::new(&source_path), target_path) {
        return Err(refused_target(format!(
            "Target path is inside the instance: {target_path}"
        )));
    }
    let (missing, existing) = missing_parents(target)?;
    match existing {
        None => {
            return Err(refused_target(format!(
                "None of the directories above the target path exist: {target_path}"
            )));
        }
        Some(existing) if !existing.as_os_str().is_empty() && !existing.is_dir() => {
            return Err(refused_target(format!(
                "Target path is below {}, which is not a directory: {target_path}",
                existing.display()
            )));
        }
        Some(existing) if leads_into_instance(existing, Path::new(&source_path))? => {
            return Err(refused_target(format!(
                "Target path is inside the instance, which {} leads into: {target_path}",
                existing.display()
            )));
        }
        Some(_) => {}
    }
    crate::repository::refuse_instance_nested_at(target)?;

    let vfs = require_own_mount(instance_id, &source_path, "move")?;
    if vfs.is_virtual()
        && let Some(mm) = MountManagerState::mount_manager()
        && mm.is_mounted(target).unwrap_or(false)
    {
        return Err(refused_target(format!(
            "Target path is already an instance mount: {target_path}"
        )));
    }
    require_working_directory_outside(&source_path, "move")?;
    // The new path is recorded through the instance's config and, if it has one, its shared
    // store, so a problem with either is found before anything is moved.
    uses_shared_store(&repository).await?;

    // Emit begin event
    LoreEvent::RepositoryInstanceMoveBegin(LoreRepositoryInstanceMoveBeginEventData {
        instance_id,
        source_path: LoreString::from_str(&source_path),
        target_path: LoreString::from_str(target_path),
    })
    .send();

    if dry_run {
        LoreEvent::RepositoryInstanceMoveComplete(LoreRepositoryInstanceMoveCompleteEventData {
            instance_id,
            target_path: LoreString::from_str(target_path),
        })
        .send();
        return Ok(());
    }

    create_parents(&missing, target_path).await?;
    if let Err(error) = move_instance(vfs, instance_id, &source_path, target_path).await {
        remove_created_parents(&missing).await;
        return Err(error);
    }

    if let Err(error) = register(target).await {
        // A failed write can still have left part of the new record behind, so moving the
        // instance back is not enough: its registration has to name the source again.
        let mut message = format!("Failed to record the new path {target_path}: {error}");
        match move_instance(vfs, instance_id, target_path, &source_path).await {
            Ok(()) => {
                match register(Path::new(&source_path)).await {
                    Ok(()) => remove_left_mount_point(vfs, target_path).await,
                    Err(restore_error) => {
                        message.push_str(&format!(
                            ". The instance is back at {source_path}, but its registration could \
                             not be restored: {restore_error}. Run `lore repository update-path` \
                             in it"
                        ));
                    }
                }
                remove_created_parents(&missing).await;
            }
            Err(undo_error) if vfs == VfsType::Fuse => {
                message.push_str(&format!(
                    ". Moving the instance back to {source_path} also failed: {undo_error}. \
                     Restarting the service mounts it at the path its registration names"
                ));
            }
            Err(undo_error) => {
                message.push_str(&format!(
                    ". Moving the instance back to {source_path} also failed: {undo_error}. It \
                     is at {target_path}; run `lore repository update-path` in it"
                ));
            }
        }
        lore_warn!("{message}");
        return Err(InstanceError::internal(message));
    }
    remove_left_mount_point(vfs, &source_path).await;

    LoreEvent::RepositoryInstanceMoveComplete(LoreRepositoryInstanceMoveCompleteEventData {
        instance_id,
        target_path: LoreString::from_str(target_path),
    })
    .send();
    lore_debug!("Moved instance {instance_id} from {source_path} to {target_path}");
    Ok(())
}

/// A file a clone writes into the Lore directory of its in-progress directory, before anything
/// is copied, and removes once the clone is at its destination: the mark of a directory a later
/// clone to the same destination removes.
#[lore_macro::test_pub]
pub(crate) const CLONE_IN_PROGRESS_MARK: &str = "clone-in-progress";

/// What the name of a clone's in-progress directory puts between the destination's name and the
/// clone's instance ID.
const IN_PROGRESS_INFIX: &str = ".cloning-";

/// The longest name of an entry in a directory, in bytes, that the filesystems instances live on
/// allow: 255 on Unix filesystems, and 255 UTF-16 code units on Windows, which never take more
/// bytes than the name's encoding here does.
const MAX_NAME_LENGTH: usize = 255;

/// The anchors a clone takes from its source, in the order [`clone_anchors`] reads them.
const CLONED_ANCHORS: [&str; 3] = [ANCHOR_CURRENT, ANCHOR_CURRENT_BRANCH, ANCHOR_STAGED];

/// `path` as a clone reports and registers it: cleaned, with forward separators and no Windows
/// verbatim prefix.
fn display_path(path: &Path) -> String {
    crate::util::path::clean(path.to_string_lossy().into_owned())
}

/// Where a clone at `target_path` can be created: its resolved parent joined with its name.
///
/// Refused with [`InstanceError::InvalidArguments`]: a destination that exists, one whose parent
/// does not, one that names no directory, and one nested in another instance or in a virtual
/// instance's mount.
fn resolve_clone_destination(target_path: &str) -> Result<PathBuf, InstanceError> {
    if target_path.is_empty() {
        return Err(refused_target("no target path given".to_string()));
    }
    let refused = |reason: &str| refused_target(format!("{target_path}: {reason}"));
    let destination =
        crate::util::path::make_absolute(target_path).map_err(|err| refused(&err.to_string()))?;
    let (Some(parent), Some(name)) = (destination.parent(), destination.file_name()) else {
        return Err(refused("it names no directory to create"));
    };
    let parent = parent
        .canonicalize()
        .map_err(|err| refused(&format!("its parent cannot be resolved: {err}")))?;
    // The longest name the clone gives a path beside the destination: a removal's, which ends
    // in [`REMOVING_SUFFIX`] after the in-progress name and its 32 hexadecimal digits.
    let longest = name.len() + IN_PROGRESS_INFIX.len() + 32 + REMOVING_SUFFIX.len();
    if longest > MAX_NAME_LENGTH {
        return Err(refused(&format!(
            "its name is too long for the directory the clone writes beside it, whose name \
             adds {} characters",
            longest - name.len()
        )));
    }
    let destination = parent.join(name);
    if destination.symlink_metadata().is_ok() {
        return Err(refused("it already exists"));
    }
    crate::repository::refuse_instance_nested_at(&destination)?;
    Ok(destination)
}

/// The current revision, branch and staged state of `instance`, which a clone of it takes. The
/// revision is zero before the instance's first commit, and the staged state where nothing is
/// staged; the branch is always set.
async fn clone_anchors(
    repository: &RepositoryContext,
    instance: InstanceId,
) -> Result<[Hash; 3], InstanceError> {
    let mut anchors = [Hash::default(); 3];
    for (anchor, function) in anchors.iter_mut().zip(CLONED_ANCHORS) {
        *anchor = load_anchor(repository, function, instance)
            .await
            .forward_with::<InstanceError, _>(|| {
                format!("reading the {function} of instance {instance}")
            })?
            .unwrap_or_default();
    }
    if anchors[1].is_zero() {
        return Err(InstanceError::internal(format!(
            "Instance {instance} has no branch to give its clone"
        )));
    }
    Ok(anchors)
}

/// Gives instance `clone` the `anchors` [`clone_anchors`] read.
async fn store_anchors(
    repository: &RepositoryContext,
    clone: InstanceId,
    anchors: [Hash; 3],
) -> Result<(), InstanceError> {
    let handle = repository.try_write_mutable_store().ok_or(WriteRequired)?;
    for (function, value) in CLONED_ANCHORS.into_iter().zip(anchors) {
        let (key, key_type) = anchor_key(repository.salt(), function, clone);
        handle
            .store(repository.id, key, value, key_type)
            .await
            .forward::<InstanceError>("storing the clone's anchors")?;
    }
    lore_debug!("Instance {clone} took its source's anchors");
    Ok(())
}

/// Moves the clone in progress at `in_progress` to `destination`, retrying while something has
/// its files in use, as a move of an instance does. Anything at `destination`, an empty directory
/// included, fails the move with [`io::ErrorKind::AlreadyExists`] rather than being replaced.
#[lore_macro::test_pub]
async fn move_clone_into_place(in_progress: &Path, destination: &Path) -> io::Result<()> {
    let action = format!(
        "move {} to {}",
        display_path(in_progress),
        display_path(destination)
    );
    retry_while_in_use(
        &action,
        |error: &io::Error| rename_is_retryable(error, destination),
        || {
            let (from, to) = (in_progress.to_path_buf(), destination.to_path_buf());
            async move {
                lore_base::lore_spawn_blocking!(move || lore_io::rename::rename_no_replace(
                    &from, &to
                ))
                .await
                .unwrap_or_else(|err| Err(io::Error::other(err)))
            }
        },
    )
    .await
}

/// The directory a clone to `destination` that gives its copy `clone_id` is written into before
/// it takes the destination: `<name>.cloning-<clone ID>`, beside it.
fn in_progress_path(destination: &Path, clone_id: InstanceId) -> PathBuf {
    sibling_with_suffix(destination, &format!("{IN_PROGRESS_INFIX}{clone_id}"))
}

/// What a removal of an in-progress directory appends to its name as it renames it aside, so a
/// removal cut short leaves a directory the next clone removes whatever is left in it.
const REMOVING_SUFFIX: &str = ".removing";

/// The name an in-progress directory is renamed to while it is removed.
fn removing_path(in_progress: &Path) -> PathBuf {
    sibling_with_suffix(in_progress, REMOVING_SUFFIX)
}

/// Whether `name`, beside a clone's destination named `destination`, is the name
/// [`in_progress_path`] gives: the destination's name, `.cloning-`, and a `UUIDv7` instance ID.
/// The destination's name is matched as spelled, so a leftover is removed only by a clone to the
/// destination spelled as the interrupted clone spelled it, and a name that is not UTF-8 is never
/// taken for one.
#[lore_macro::test_pub]
fn names_clone_in_progress(destination: &std::ffi::OsStr, name: &std::ffi::OsStr) -> bool {
    let (Some(destination), Some(name)) = (destination.to_str(), name.to_str()) else {
        return false;
    };
    name.strip_prefix(destination)
        .and_then(|rest| rest.strip_prefix(IN_PROGRESS_INFIX))
        .and_then(InstanceId::from_hex)
        .is_some_and(|id| uuid::Uuid::from_bytes(*id.data()).get_version_num() == 7)
}

/// A clone of an instance: planned by [`instance_clone_plan`] before the repository lock is taken, made by
/// [`instance_clone`] under the locks, and finished by [`instance_clone_finish`] after it.
pub struct InstanceClonePlan {
    provider: Arc<dyn FilesystemProvider>,
    /// The instance the plan was made for, which the clone refuses to copy any other in place of.
    source_id: InstanceId,
    source: PathBuf,
    dot_path: PathBuf,
    destination: PathBuf,
    /// The destination as the clone registers and reports it.
    target_path: String,
    /// The instance ID the copy takes.
    clone_id: InstanceId,
    /// Where the clone is written before it takes the destination, beside it.
    in_progress: PathBuf,
    /// Held from the plan to the finish, so another clone to the destination never takes this
    /// clone's in-progress directory for one an interrupted clone left.
    in_progress_lock: Arc<InProgressLock>,
    answer: CloneAnswer,
    allow_copy: bool,
}

#[cfg(feature = "test-util")]
impl InstanceClonePlan {
    /// Where the clone is written before it takes the destination.
    pub fn in_progress(&self) -> &Path {
        &self.in_progress
    }

    /// The instance ID the copy takes.
    pub fn clone_id(&self) -> InstanceId {
        self.clone_id
    }
}

/// Refuses a clone from `source` to `target` whose `answer` does not share blocks, unless
/// `allow_copy` opts into the copy it describes, with [`InstanceError::NotSupported`], and a copy
/// the destination has no room for with [`InstanceError::Oversized`]. An unsupported clone is
/// always refused.
#[lore_macro::test_pub]
fn refuse_clone(
    answer: &CloneAnswer,
    allow_copy: bool,
    source: &str,
    target: &str,
) -> Result<(), InstanceError> {
    use lore_base::units::format_bytes_to_string;

    match answer {
        CloneAnswer::Unsupported { reason } => Err(NotSupported {
            operation: format!("cloning {source} to {target}: {reason}"),
        }
        .into()),
        CloneAnswer::CopiesBytes { reason, size, .. } if !allow_copy => Err(NotSupported {
            operation: format!(
                "cloning {source} to {target} by sharing data blocks: {reason}. Put the \
                 instances on a filesystem that clones files (Dev Drive or ReFS on Windows, APFS \
                 on macOS, Btrfs, XFS or bcachefs on Linux), with the destination on the source's volume, \
                 or allow a copy, which takes {} for {} files and holds the store's locks \
                 throughout",
                format_bytes_to_string(size.size),
                size.file_count
            ),
        }
        .into()),
        CloneAnswer::CopiesBytes {
            size, available, ..
        } if *available < size.size => Err(Oversized {
            context: format!(
                "copying {source} to {target} takes {}, and the destination's volume has {} free",
                format_bytes_to_string(size.size),
                format_bytes_to_string(*available)
            ),
        }
        .into()),
        CloneAnswer::SharesBlocks { .. } | CloneAnswer::CopiesBytes { .. } => Ok(()),
    }
}

fn send_clone_progress(phase: LoreRepositoryInstanceClonePhase, path: &str, stats: CloneStats) {
    LoreEvent::RepositoryInstanceCloneProgress(LoreRepositoryInstanceCloneProgressEventData {
        phase,
        path: LoreString::from_str(path),
        cloned_directory_count: stats.cloned_directory_count,
        cloned_entry_count: stats.cloned_entry_count,
        copied_file_count: stats.copied_file_count,
    })
    .send();
}

/// Whether anything is at `path`.
async fn is_present(path: &Path) -> bool {
    lore_io::IoDriver::global().symlink_stat(path).await.is_ok()
}

/// Releases the lock of the clone in progress at `in_progress` and removes its lock file.
async fn release_in_progress_lock(in_progress: &Path, lock: FSLock) {
    let named = in_progress.to_path_buf();
    let lock_file = lore_base::lore_spawn_blocking!(move || FSLock::lock_file_of(&named)).await;
    drop(lock);
    if let Ok(Ok(lock_file)) = lock_file {
        let _ = lore_io::IoDriver::global().remove_file(lock_file).await;
    }
}

/// Removes the in-progress directories that interrupted clones to `destination` left beside it,
/// with their lock files, keeping `own` and any whose lock another clone holds. A directory is
/// removed only where its name is one [`names_clone_in_progress`] accepts and its Lore directory
/// holds [`CLONE_IN_PROGRESS_MARK`], or where its name is the one a removal renames such a
/// directory aside to, [`removing_path`], which is known by its name alone; so nothing beside the
/// destination that a clone did not write is touched.
async fn remove_leftover_clones(destination: &Path, own: &Path) {
    let (Some(parent), Some(name)) = (destination.parent(), destination.file_name()) else {
        return;
    };
    let (parent, name) = (parent.to_path_buf(), name.to_owned());
    let listed = lore_base::lore_spawn_blocking!(move || -> io::Result<Vec<PathBuf>> {
        let mut found = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&parent)? {
            let entry_name = entry?.file_name();
            let Some(text) = entry_name.to_str() else {
                continue;
            };
            let text = text.strip_suffix(FSLock::FILE_LOCK_SUFFIX).unwrap_or(text);
            let text = text.strip_suffix(REMOVING_SUFFIX).unwrap_or(text);
            if names_clone_in_progress(&name, text.as_ref()) {
                found.insert(parent.join(text));
            }
        }
        Ok(found.into_iter().collect())
    })
    .await;
    let leftovers = match listed {
        Ok(Ok(leftovers)) => leftovers,
        Ok(Err(err)) => {
            lore_warn!(
                "Failed to list the directory holding {} for clones an interruption left: {err}",
                destination.display()
            );
            return;
        }
        Err(err) => {
            lore_warn!(
                "Failed to list the directory holding {} for clones an interruption left: {err}",
                destination.display()
            );
            return;
        }
    };
    for in_progress in leftovers.into_iter().filter(|path| path != own) {
        let removing = removing_path(&in_progress);
        // Checked before the lock is taken too, so a directory no clone left gets no lock file.
        if !is_present(&removing).await && !is_marked_in_progress(&in_progress).await {
            continue;
        }
        let locking = in_progress.clone();
        let Ok(Ok(Some(lock))) =
            lore_base::lore_spawn_blocking!(move || FSLock::try_acquire_file_lock(&locking)).await
        else {
            continue;
        };
        // The lock file is removed only with a directory this removed: one with nothing to remove
        // may belong to a clone that has created it and not yet taken it, and removing it would let
        // a third clone lock a new file beside that clone's live directory.
        let removed = if is_present(&removing).await {
            remove_clone_left(&removing, None).await
        } else if is_present(&in_progress).await && is_marked_in_progress(&in_progress).await {
            remove_clone_left(&in_progress, Some(&removing)).await
        } else {
            false
        };
        if removed {
            release_in_progress_lock(&in_progress, lock).await;
        }
    }
}

/// Whether the Lore directory of the directory at `path` holds [`CLONE_IN_PROGRESS_MARK`].
async fn is_marked_in_progress(path: &Path) -> bool {
    for dot in [crate::repository::DOT_LORE, crate::repository::DOT_URC] {
        if is_present(&path.join(dot).join(CLONE_IN_PROGRESS_MARK)).await {
            return true;
        }
    }
    false
}

/// Removes `path`, which an interrupted clone left, renamed aside to `aside` first where given,
/// with a `RemovingLeftover` progress event, and says whether it is gone.
async fn remove_clone_left(path: &Path, aside: Option<&Path>) -> bool {
    send_clone_progress(
        LoreRepositoryInstanceClonePhase::RemovingLeftover,
        &display_path(path),
        CloneStats::default(),
    );
    remove_clone_directory(path, aside, "an interrupted clone").await
}

/// Removes `path`, a directory `left_by` left, renamed aside to `aside` first where given, and
/// says whether it is gone, warning where it is not.
async fn remove_clone_directory(path: &Path, aside: Option<&Path>, left_by: &str) -> bool {
    match remove_instance_directory(path, aside, true).await {
        Ok(()) => true,
        Err(failure) => {
            lore_warn!("{failure}, which {left_by} left");
            false
        }
    }
}

/// Plans a clone of the current instance to `target_path` before any lock is taken: runs the
/// checks, asks the instance's filesystem provider what the clone would do, sends
/// `RepositoryInstanceCloneBegin` with its answer, and refuses a clone that cannot share blocks,
/// unless `allow_copy` opts into a copy that fits at the destination.
///
/// Refused: a destination that exists, whose parent does not, or that is nested in another
/// instance or a virtual instance's mount, with [`InstanceError::InvalidArguments`]; and an
/// instance on a local store, since the clone's anchors and registration would be written to the
/// source's own store, with [`InstanceError::NotSupported`].
///
/// In a dry run (the global `dry_run` flag), stops there and returns `None`. Otherwise removes
/// the in-progress directories interrupted clones to the destination left, and returns the plan,
/// holding the lock of its own in-progress directory.
pub async fn instance_clone_plan(
    repository: Arc<RepositoryContext>,
    target_path: &str,
    allow_copy: bool,
) -> Result<Option<InstanceClonePlan>, InstanceError> {
    instance_clone_plan_with(
        repository.file_system(),
        repository,
        target_path,
        allow_copy,
    )
    .await
}

/// [`instance_clone_plan`] through `provider` in place of the instance's own.
#[lore_macro::test_pub]
async fn instance_clone_plan_with(
    provider: Arc<dyn FilesystemProvider>,
    repository: Arc<RepositoryContext>,
    target_path: &str,
    allow_copy: bool,
) -> Result<Option<InstanceClonePlan>, InstanceError> {
    let dry_run = crate::lore::execution_context().globals().dry_run();
    let instance_id = repository.instance_id;
    let source = repository.require_path()?.to_path_buf();
    let dot_path = repository.dot_dir_path()?.to_path_buf();
    if !uses_shared_store(&repository).await? {
        return Err(NotSupported {
            operation: "cloning an instance with a local store, since the clone's anchors and \
                        registration would be written to the source's own store; only an \
                        instance on a shared store can be cloned"
                .to_string(),
        }
        .into());
    }
    // Resolving the destination stats each directory above it, so it runs off the runtime.
    let target = target_path.to_string();
    let destination = lore_base::lore_spawn_blocking!(move || resolve_clone_destination(&target))
        .await
        .internal("resolving the clone's destination")??;
    let target_path = display_path(&destination);
    let source_path = display_path(&source);
    let answer = provider
        .clone_answer(&source, &dot_path, &destination)
        .await
        .forward_with::<InstanceError, _>(|| {
            format!("finding how {source_path} can be cloned to {target_path}")
        })?;

    let (method, volume, size, available, reason) = match &answer {
        CloneAnswer::SharesBlocks { volume, .. } => (
            LoreRepositoryInstanceCloneMethod::SharesBlocks,
            Some(volume),
            lore_io::clone::TreeSize::default(),
            0,
            "",
        ),
        CloneAnswer::CopiesBytes {
            reason,
            size,
            volume,
            available,
        } => (
            LoreRepositoryInstanceCloneMethod::CopiesBytes,
            Some(volume),
            *size,
            *available,
            reason.as_str(),
        ),
        CloneAnswer::Unsupported { reason } => (
            LoreRepositoryInstanceCloneMethod::Unsupported,
            None,
            lore_io::clone::TreeSize::default(),
            0,
            reason.as_str(),
        ),
    };
    LoreEvent::RepositoryInstanceCloneBegin(LoreRepositoryInstanceCloneBeginEventData {
        instance_id,
        source_path: LoreString::from_str(&source_path),
        target_path: LoreString::from_str(&target_path),
        method,
        filesystem: LoreString::from_str(volume.map_or("", |volume| volume.filesystem.as_str())),
        mount_point: LoreString::from_str(
            &volume.map_or_else(String::new, |volume| display_path(&volume.mount_point)),
        ),
        size: size.size,
        file_count: size.file_count,
        available_size: available,
        reason: LoreString::from_str(reason),
    })
    .send();
    refuse_clone(&answer, allow_copy, &source_path, &target_path)?;

    if dry_run {
        LoreEvent::RepositoryInstanceCloneComplete(LoreRepositoryInstanceCloneCompleteEventData {
            instance_id: InstanceId::default(),
            target_path: LoreString::from_str(&target_path),
            cloned_directory_count: 0,
            cloned_entry_count: 0,
            copied_file_count: 0,
        })
        .send();
        return Ok(None);
    }

    let clone_id = InstanceId::generate();
    let in_progress = in_progress_path(&destination, clone_id);
    // The lock file's name holds the new instance ID, so no other clone holds it; the lock is
    // taken off the runtime, as resolving its path stats the directory it lies in.
    let locking = in_progress.clone();
    let in_progress_lock =
        lore_base::lore_spawn_blocking!(move || FSLock::try_acquire_file_lock(&locking))
            .await
            .internal("locking the clone's in-progress directory")?
            .internal("locking the clone's in-progress directory")?
            .ok_or_else(|| {
                InstanceError::internal(format!(
                    "{}: another clone holds the lock of this clone's in-progress directory",
                    display_path(&in_progress)
                ))
            })?;
    remove_leftover_clones(&destination, &in_progress).await;
    Ok(Some(InstanceClonePlan {
        provider,
        source_id: instance_id,
        source,
        dot_path,
        destination,
        target_path,
        clone_id,
        in_progress_lock: Arc::new(InProgressLock {
            _lock: in_progress_lock,
            in_progress: in_progress.clone(),
        }),
        in_progress,
        answer,
        allow_copy,
    }))
}

/// Clones the current instance as `plan` describes: copies its directory, `.lore` included,
/// through the provider's clone backend, and registers the copy as an instance of its own at the
/// source's current revision, branch and staged state.
///
/// The anchors are read, and a destination another live instance is registered at is refused,
/// before the clone writes anything. The copy is written to the plan's in-progress directory,
/// whose Lore directory is marked as a clone in progress, names the copy's new ID and is marked
/// as holding no store records before anything else is copied. The modified-time records are
/// written back first, so the copy's first scan hashes only the files a scan of the source would:
/// those with no record, and those changed since theirs. A source moved or deleted during the copy
/// fails the clone: `instance move` and `instance delete` take no repository lock, so the lock does
/// not keep them out, though on Windows the copy's hold on the directory it lists refuses them. The anchors are written to the shared store, and synced to disk,
/// before the copy takes the destination, and the registration, which retires other
/// registrations at the destination, only once it has. A failure before the move removes the
/// clone's keys as far as the store allows; a crash before it leaves the clone's three anchor keys
/// and no registration, and a crash after it an instance that registers itself when a write
/// command next opens it. The copied files' data is not synced: after a power loss a copied file
/// can hold less than its kept time vouches for, as after any copy the system has not written
/// out. A failure names the in-progress directory and leaves it for [`instance_clone_finish`] to
/// remove.
///
/// The caller holds the instance's repository lock alone, and with it the store's locks, for
/// the whole call (see [`crate::repository::load_and_connect_exclusive`]), so no other command
/// that opens the instance's store changes the instance while it is cloned.
pub async fn instance_clone(
    repository: Arc<RepositoryContext>,
    plan: &InstanceClonePlan,
) -> Result<(), InstanceError> {
    instance_clone_with(repository, plan, move_clone_into_place, register_clone).await
}

/// [`instance_clone`] with the step that moves the copy to its destination taken from
/// `move_into_place`, which answers as [`move_clone_into_place`] does.
#[lore_macro::test_pub]
async fn instance_clone_with(
    repository: Arc<RepositoryContext>,
    plan: &InstanceClonePlan,
    move_into_place: impl AsyncFn(&Path, &Path) -> io::Result<()>,
    register: impl AsyncFn(&Arc<RepositoryContext>, InstanceId, &str) -> Result<(), InstanceError>,
) -> Result<(), InstanceError> {
    let instance_id = repository.instance_id;
    let context = || {
        format!(
            "cloning to {}, in progress at {}",
            plan.target_path,
            display_path(&plan.in_progress)
        )
    };
    // An instance moved to the source's path since the plan is not the tree the plan measured.
    if instance_id != plan.source_id {
        return Err(FileNotFound {
            resource: format!(
                "{}: instance {} was planned for, and instance {instance_id} is there now",
                plan.source.display(),
                plan.source_id
            ),
        }
        .into());
    }
    let anchors = clone_anchors(&repository, instance_id)
        .await
        .forward_with::<InstanceError, _>(context)?;
    refuse_registered_destination(&repository, &plan.target_path)
        .await
        .forward_with::<InstanceError, _>(context)?;

    send_clone_progress(
        LoreRepositoryInstanceClonePhase::WritingRecords,
        "",
        CloneStats::default(),
    );
    repository
        .write_back_instance_records()
        .await
        .map_err(|err| clone_io_failure(err, "writing the modified-time records back"))
        .forward_with::<InstanceError, _>(context)?;

    send_clone_progress(
        LoreRepositoryInstanceClonePhase::Copying,
        "",
        CloneStats::default(),
    );
    let stats = copy_instance_directory(&repository, plan)
        .await
        .forward_with::<InstanceError, _>(context)?;
    require_source_in_place(plan, instance_id)
        .await
        .forward_with::<InstanceError, _>(context)?;

    send_clone_progress(LoreRepositoryInstanceClonePhase::Registering, "", stats);
    let flush_options = crate::lore::execution_context().globals().flush_options();
    let placed = async {
        store_anchors(&repository, plan.clone_id, anchors).await?;
        // Synced whatever `--sync-data` says, so the rename never lands on disk before them.
        repository
            .flush(FlushOptions::SYNC_DATA)
            .await
            .forward_any::<InstanceError>("flushing the clone's anchors")?;
        match move_into_place(&plan.in_progress, &plan.destination).await {
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                Err(refused_target(format!(
                    "{}: it already exists",
                    plan.target_path
                )))
            }
            moved => moved
                .map_err(|err| clone_io_failure(err, "moving the clone to its destination"))
                .forward::<InstanceError>("moving the clone to its destination"),
        }
    }
    .await;
    if let Err(err) = placed {
        remove_instance_keys(&repository, plan.clone_id).await;
        let _ = repository.flush(flush_options).await;
        return Err(err).forward_with::<InstanceError, _>(context);
    }
    if let Ok(dot_name) = lore_directory_within(&plan.source, &plan.dot_path)
        && let Err(err) = lore_io::IoDriver::global()
            .remove_file(plan.destination.join(dot_name).join(CLONE_IN_PROGRESS_MARK))
            .await
    {
        lore_warn!(
            "Failed to remove the mark of a clone in progress from {}: {err}",
            plan.target_path
        );
    }
    // A registration synced to disk never names a destination the move has not reached there.
    if flush_options.sync_data {
        let destination = plan.destination.clone();
        match lore_base::lore_spawn_blocking!(move || sync_parent_directory(&destination)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => lore_warn!(
                "Failed to sync the directory holding {}: {err}",
                plan.target_path
            ),
            Err(err) => lore_warn!(
                "Failed to sync the directory holding {}: {err}",
                plan.target_path
            ),
        }
    }
    register(&repository, plan.clone_id, &plan.target_path)
        .await
        .forward_with::<InstanceError, _>(|| {
            format!(
                "registering the clone at {}, which registers itself when a write command next \
                 opens it",
                plan.target_path
            )
        })?;

    LoreEvent::RepositoryInstanceCloneComplete(LoreRepositoryInstanceCloneCompleteEventData {
        instance_id: plan.clone_id,
        target_path: LoreString::from_str(&plan.target_path),
        cloned_directory_count: stats.cloned_directory_count,
        cloned_entry_count: stats.cloned_entry_count,
        copied_file_count: stats.copied_file_count,
    })
    .send();
    lore_debug!(
        "Cloned instance {instance_id} at {} to {} as {}",
        plan.source.display(),
        plan.target_path,
        plan.clone_id
    );
    Ok(())
}

/// Syncs the directory that holds `path` to disk, so a rename into it survives a power loss.
/// Windows has no call that syncs a directory, so there it does nothing.
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(target_family = "unix")]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(target_family = "windows")]
    let _ = path;
    Ok(())
}

/// Refuses a clone to `target_path` while another instance that is not stale is registered
/// there, as an SWFS instance outside its service is at a path that does not exist:
/// registering the clone would retire it.
async fn refuse_registered_destination(
    repository: &Arc<RepositoryContext>,
    target_path: &str,
) -> Result<(), InstanceError> {
    let instances = list_instances(repository)
        .await
        .forward::<InstanceError>("listing the instances registered at the destination")?;
    for instance in instances {
        if crate::util::path::clean(instance.path.clone()) == target_path
            && instance_staleness(&instance.path, instance.instance_id).await
                == InstanceStaleness::Active
        {
            return Err(refused_target(format!(
                "{target_path}: instance {} is registered there",
                instance.instance_id
            )));
        }
    }
    Ok(())
}

/// Fails where the source's Lore directory no longer names `instance_id`, as after the source
/// was moved or deleted while it was copied: the copy may then lack part of the tree.
async fn require_source_in_place(
    plan: &InstanceClonePlan,
    instance_id: InstanceId,
) -> Result<(), InstanceError> {
    let named = match lore_io::IoDriver::global()
        .read_file_bytes(plan.dot_path.join(crate::repository::INSTANCE))
        .await
    {
        Ok(bytes) => InstanceId::read_from_bytes(&bytes).ok(),
        Err(err) if is_absent(&err) => None,
        Err(err) => {
            return Err(InstanceError::internal_with_context(
                err,
                "reading the instance file after the copy",
            ));
        }
    };
    if named == Some(instance_id) {
        return Ok(());
    }
    Err(FileNotFound {
        resource: format!(
            "{}: the instance was moved or deleted while it was cloned",
            plan.source.display()
        ),
    }
    .into())
}

/// Registers instance `clone` at `target_path`, retiring other registrations there, and flushes
/// the registration.
#[lore_macro::test_pub]
async fn register_clone(
    repository: &Arc<RepositoryContext>,
    clone: InstanceId,
    target_path: &str,
) -> Result<(), InstanceError> {
    register_instance(repository, clone, target_path)
        .await
        .forward::<InstanceError>("registering the clone")?;
    let flush_options = crate::lore::execution_context().globals().flush_options();
    repository
        .flush(flush_options)
        .await
        .forward_any::<InstanceError>("flushing the clone's registration")
}

/// Finishes a clone after [`instance_clone`], whatever it returned: removes the in-progress
/// directory a failure left, at background disk priority and retrying while its files are in
/// use, and the in-progress directory's lock file. The command waits for the removal, which
/// takes minutes for a large tree; one cut short leaves the directory for the next clone to the
/// same destination to remove.
pub async fn instance_clone_finish(plan: InstanceClonePlan) {
    // Shared only with a copy a provider left running after `clone_instance` returned, which
    // keeps the directory, and the lock file, for the next clone to the destination to remove.
    let Some(lock) = Arc::into_inner(plan.in_progress_lock) else {
        return;
    };
    if is_present(&plan.in_progress).await
        && !remove_clone_directory(
            &plan.in_progress,
            Some(&removing_path(&plan.in_progress)),
            "a failed clone",
        )
        .await
    {
        return;
    }
    // With the directory gone, dropping the lock stats and removes its file: blocking IO.
    let _ = lore_base::lore_spawn_blocking!(move || drop(lock)).await;
}

/// The lock of a clone's in-progress directory, held from the plan to the finish. Dropped with
/// no in-progress directory left, as when a clone is cancelled before its copy starts or once
/// the finish has removed the directory, it removes its lock file too; dropped while the
/// in-progress path itself still exists, it leaves the file for the clone that removes the
/// directory.
struct InProgressLock {
    /// Held for the value's life and released after the lock file is removed.
    _lock: FSLock,
    in_progress: PathBuf,
}

impl Drop for InProgressLock {
    // The lock file belongs to this clone, and no repository file is written.
    #[allow(clippy::disallowed_methods)]
    fn drop(&mut self) {
        // A stat and an unlink, run once as a clone ends, and run while the lock is still held,
        // as the field drops after this.
        if std::fs::symlink_metadata(&self.in_progress).is_err()
            && let Ok(file) = FSLock::lock_file_of(&self.in_progress)
        {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// Copies the instance directory as `plan` describes to its in-progress directory. The copy's
/// Lore directory is first given the mark of a clone in progress, the clone's instance file and
/// the mark of an instance holding no store records, and the copy is then made through the
/// plan's provider, with a `Copying` progress event sent with its counts each event interval.
async fn copy_instance_directory(
    repository: &Arc<RepositoryContext>,
    plan: &InstanceClonePlan,
) -> Result<CloneStats, InstanceCloneError> {
    let dot = plan
        .in_progress
        .join(lore_directory_within(&plan.source, &plan.dot_path)?);
    let io = lore_io::IoDriver::global();
    io.create_dir_all(&dot)
        .await
        .map_err(|err| clone_io_failure(err, "creating the clone's Lore directory"))?;
    io.write_file_bytes(dot.join(CLONE_IN_PROGRESS_MARK), bytes::Bytes::new(), false)
        .await
        .map_err(|err| clone_io_failure(err, "marking the clone as in progress"))?;
    plan.clone_id
        .write_to_file(dot.join(crate::repository::INSTANCE))
        .await
        .map_err(|err| clone_io_failure(err, "writing the clone's instance ID"))?;
    crate::records::mark_holding_no_store_records(&dot)
        .await
        .map_err(|err| clone_io_failure(err, "marking the clone as holding no store records"))?;

    let started = std::time::Instant::now();
    // The copy keeps the in-progress directory's lock and the instance's locks, through the
    // repository context, until it ends, should this future be dropped before it.
    let run = crate::fs::filesystem_provider::CloneRun {
        progress: Arc::new(lore_io::clone::Progress::default()),
        hold: Arc::new((Arc::clone(&plan.in_progress_lock), Arc::clone(repository))),
    };
    let clone = std::pin::pin!(plan.provider.clone_instance(
        &plan.source,
        &plan.dot_path,
        &plan.in_progress,
        &plan.answer,
        plan.allow_copy,
        &run,
    ));
    let stats = report_copy_progress(clone, &run.progress).await?;
    lore_debug!(
        "Cloned the instance to {} in {:.1}s ({} directories cloned whole, {} entries cloned, {} \
         files copied)",
        display_path(&plan.in_progress),
        started.elapsed().as_secs_f64(),
        stats.cloned_directory_count,
        stats.cloned_entry_count,
        stats.copied_file_count
    );
    Ok(stats)
}

/// Awaits `clone`, sending a `Copying` progress event with the counts of `progress` each event
/// interval the call's globals set, until it ends.
async fn report_copy_progress<T>(
    mut clone: std::pin::Pin<&mut impl Future<Output = T>>,
    progress: &lore_io::clone::Progress,
) -> T {
    let mut each_interval =
        tokio::time::interval(crate::lore::execution_context().globals().event_interval());
    each_interval.tick().await;
    loop {
        tokio::select! {
            done = &mut clone => return done,
            _ = each_interval.tick() => send_clone_progress(
                LoreRepositoryInstanceClonePhase::Copying,
                "",
                progress.stats(),
            ),
        }
    }
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
