// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;

use lore::Context;
use lore::Hash;
use lore_base::allocator::GrowVec;
use lore_base::lore_spawn;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
// Re-export from lore-storage for internal use
pub(crate) use lore_storage::local::mutable_store::*;
use lore_transport::Connection;
use tokio::task::JoinSet;

use crate::branch;
use crate::errors::AddressNotFound;
use crate::lore;
use crate::lore::Address;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_info;
use crate::metadata;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::store;
use crate::store::ImmutableStore;
use crate::store::KeyType;
use crate::store::StoreError;
use crate::store::StoreMatch;

// Backward compatibility aliases
#[error_set]
pub enum MutableStoreError {}

pub(crate) type MutableStore = lore_storage::local::mutable_store::LocalMutableStore;

fn set_key_type(hash: &mut Hash, key_type: KeyType) {
    hash.data_mut()[2] = key_type as u8;
}

fn write_u32(mut file: &std::fs::File, value: u32) -> io::Result<()> {
    use std::io::Write;
    let buf = u32::to_ne_bytes(value);
    file.write_all(&buf)
}

pub async fn upgrade(
    store: &MutableStore,
    immutable_store: Arc<dyn ImmutableStore>,
    remote: Option<&Arc<Connection>>,
    repository_id: RepositoryId,
) -> Result<(), MutableStoreError> {
    if !store.needs_upgrade() {
        return Ok(());
    }

    let remote = remote.ok_or_else(|| {
        MutableStoreError::internal("Mutable store upgrade requires a remote connection")
    })?;

    let path = store
        .path
        .as_deref()
        .ok_or_else(|| MutableStoreError::internal("No path for mutable store upgrade"))?;

    migrate_initial_to_typed(path, immutable_store, remote, repository_id).await?;

    // Write TypedItems version
    let version_path = path.join("version");
    let version_file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(&version_path)
        .map_err(|err| {
            MutableStoreError::internal_with_context(err, "Failed to upgrade mutable store")
        })?;
    write_u32(&version_file, MutableStoreVersion::TypedItems as u32).map_err(|err| {
        MutableStoreError::internal_with_context(err, "Failed to upgrade mutable store")
    })?;

    store.needs_upgrade.store(false, atomic::Ordering::Relaxed);
    Ok(())
}

async fn migrate_initial_to_typed(
    path: &Path,
    immutable_store: Arc<dyn ImmutableStore>,
    remote: &Arc<Connection>,
    repository_id: RepositoryId,
) -> Result<(), MutableStoreError> {
    // Upgrade strategy:
    // Deserialize all buckets. For each bucket, iterate all entries.
    // For each entry check the value. If the value can be loaded from the
    // immutable store, check the size and determine which kind of data it is.
    // If not loadable, check remote storage. Branch and repository name->id
    // mappings, which have a zero padded context value, will be upgraded by the
    // repository and branch metadata upgrade path.
    lore_info!("Upgrading mutable store from initial to typed items");

    let mut old_store = MutableStore {
        path: Some(Arc::new(path.to_path_buf())),
        lock: None,
        group: Vec::with_capacity(GROUP_COUNT),
        flush_delay_seconds: 0,
        needs_upgrade: AtomicBool::new(false),
        // Authoritative: a corrupt bucket must fail the upgrade, not be silently dropped.
        authoritative: true,
    };

    for _ in 0..GROUP_COUNT {
        old_store.group.push(Arc::new(MutableStoreGroup {
            bucket: [const { OnceLock::new() }; BUCKET_COUNT],
            dirty: std::array::from_fn(|_| AtomicBool::new(false)),
            bucket_count: std::sync::atomic::AtomicUsize::new(
                lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ),
            serialize_version: std::sync::atomic::AtomicU32::new(
                lore_storage::local::mutable_store::MutableStoreVersion::LazyFanOut as u32,
            ),
            fan_out_threshold: lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
            committed_level: std::sync::atomic::AtomicUsize::new(
                lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ),
        }));
    }

    let old_store = Arc::new(old_store);

    // Deserialize all old data, since migrations will require to look up dependent data
    let mut tasks = JoinSet::new();
    for group_index in 0..GROUP_COUNT {
        let group = old_store.group[group_index].clone();
        let path = path.to_path_buf();
        lore_spawn!(tasks, async move {
            for bucket_index in 0..BUCKET_COUNT {
                let bucket = group.bucket(bucket_index).clone();
                let mut bucket = bucket.write().await;
                bucket
                    .deserialize(&path, group_index, bucket_index, false, true)
                    .await
                    .map_err(|e| {
                        MutableStoreError::internal_with_context(e, "deserialize failed")
                    })?;
            }
            Ok(())
        });
    }

    lore_drain_tasks!(
        tasks,
        MutableStoreError::internal("Failed to migrate initial version to typed version")
    )?;

    // Create new groups for migrated data — old_store must remain unmodified for migrate_lookup
    let mut new_groups: Vec<Arc<MutableStoreGroup>> = Vec::with_capacity(GROUP_COUNT);
    for _ in 0..GROUP_COUNT {
        new_groups.push(Arc::new(MutableStoreGroup {
            bucket: [const { OnceLock::new() }; BUCKET_COUNT],
            dirty: std::array::from_fn(|_| AtomicBool::new(false)),
            bucket_count: std::sync::atomic::AtomicUsize::new(
                lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ),
            serialize_version: std::sync::atomic::AtomicU32::new(
                lore_storage::local::mutable_store::MutableStoreVersion::LazyFanOut as u32,
            ),
            fan_out_threshold: lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
            committed_level: std::sync::atomic::AtomicUsize::new(
                lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ),
        }));
    }

    // Then migrate everything from old_store into new groups
    for (group_index, new_group) in new_groups.iter().enumerate() {
        let remote = remote.clone();
        let new_group = new_group.clone();
        lore_spawn!(
            tasks,
            migrate_group_initial_to_typed(
                old_store.clone(),
                new_group,
                group_index,
                immutable_store.clone(),
                remote,
                repository_id,
            )
        );
    }

    lore_drain_tasks!(
        tasks,
        MutableStoreError::internal("Failed to migrate initial version to typed version")
    )?;

    // Then serialize migrated data if all were successful
    let store_path = old_store.path.clone().unwrap_or_default();
    for (group_index, group) in new_groups.iter().enumerate() {
        for bucket_index in 0..BUCKET_COUNT {
            let bucket = group.bucket(bucket_index).clone();
            let bucket = bucket.read_owned().await;
            MutableStoreBucket::serialize(
                bucket,
                group.clone(),
                &store_path,
                group_index,
                bucket_index,
                true,
            )
            .await
            .forward::<MutableStoreError>("serialize failed")?;
        }
    }

    Ok(())
}

async fn migrate_group_initial_to_typed(
    old_store: Arc<MutableStore>,
    new_group: Arc<MutableStoreGroup>,
    group_index: usize,
    immutable_store: Arc<dyn ImmutableStore>,
    remote: Arc<Connection>,
    repository_id: RepositoryId,
) -> Result<(), MutableStoreError> {
    let old_group = old_store.group[group_index].clone();
    for bucket_index in 0..BUCKET_COUNT {
        let old_bucket = old_group.bucket(bucket_index).clone();
        let old_bucket = old_bucket.read().await;
        if old_bucket.version < MutableStoreVersion::TypedItems as u32 {
            let new_bucket = new_group.bucket(bucket_index).clone();
            let mut new_bucket = new_bucket.write().await;
            migrate_bucket_initial_to_typed(
                &old_bucket,
                &mut new_bucket,
                immutable_store.clone(),
                old_store.clone(),
                &remote,
                repository_id,
            )
            .await?;
            new_group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        }
    }

    Ok(())
}

async fn migrate_bucket_initial_to_typed(
    old_bucket: &MutableStoreBucket,
    new_bucket: &mut MutableStoreBucket,
    immutable_store: Arc<dyn ImmutableStore>,
    old_store: Arc<MutableStore>,
    remote: &Arc<Connection>,
    repository_id: RepositoryId,
) -> Result<(), MutableStoreError> {
    for entry in old_bucket.entry.iter() {
        let mut migrated = *entry;
        let hash = entry.value;
        let address = Address {
            hash,
            context: Context::default(),
        };

        let is_local_immutable = if let Ok(result) = immutable_store
            .clone()
            .query(entry.partition, address, StoreMatch::MatchFull)
            .await
        {
            result.match_made != StoreMatch::MatchNone
        } else {
            false
        };

        if is_local_immutable {
            migrate_immutable_value_to_typed(
                &mut migrated,
                immutable_store.clone(),
                old_store.clone(),
                remote,
            )
            .await?;
        } else if hash.data()[16..] != [0; 16] {
            // Not found locally — check remote storage protocol unless it is extremely likely
            // to be a local file timestamp or a branch id
            let remote_exists = if let Ok(storage) = {
                let correlation_id = crate::lore::execution_context()
                    .globals()
                    .correlation_id
                    .to_string();
                remote.session(repository_id, &correlation_id).await
            } {
                let status = storage.query(&[address]).await.unwrap_or_default();
                !status.is_empty() && status[0] == 0
            } else {
                false
            };

            if remote_exists {
                migrate_immutable_value_to_typed(
                    &mut migrated,
                    immutable_store.clone(),
                    old_store.clone(),
                    remote,
                )
                .await?;
            } else {
                migrate_non_immutable_value_to_typed(&mut migrated, old_store.clone()).await?;
            }
        } else {
            migrate_non_immutable_value_to_typed(&mut migrated, old_store.clone()).await?;
        }

        new_bucket.entry.push(migrated);
    }

    // Build sorted_index from scratch
    let count = new_bucket.entry.len();
    let mut indices: Vec<u32> = (0..count as u32).collect();
    indices.sort_by(|&a, &b| {
        let ea = &new_bucket.entry[a as usize];
        let eb = &new_bucket.entry[b as usize];
        ea.key.cmp(&eb.key).then(ea.partition.cmp(&eb.partition))
    });
    let mut sorted_index = GrowVec::new();
    for idx in indices {
        sorted_index.push(idx);
    }
    new_bucket.sorted_index = sorted_index;

    Ok(())
}

async fn migrate_non_immutable_value_to_typed(
    entry: &mut MutableStoreEntry,
    mutable_store: Arc<MutableStore>,
) -> Result<(), MutableStoreError> {
    let hash = entry.value;
    if hash.data()[8..] == [0; 24] {
        // File timestamp
        set_key_type(&mut entry.key, KeyType::Untyped);
        return Ok(());
    }
    if hash.data()[16..] == [0; 16] {
        // Check if it is a branch name -> id mapping
        let branch_id = hash.to_context();
        {
            lore_debug!("Lookup branch metadata for {branch_id}");
            let (key, _) = branch::mutable_key(
                repository::SALT_URC,
                branch::METADATA,
                entry.partition,
                branch_id,
            );
            if let Ok(_value) = migrate_lookup(mutable_store.clone(), entry.partition, key).await {
                // The metadata existed, so initial mutable key was a branch name -> id
                lore_debug!(
                    "Mutable key {} found to be branch name -> ID: {}",
                    entry.key,
                    branch_id
                );
                set_key_type(&mut entry.key, KeyType::BranchId);
                return Ok(());
            }
        }

        // Check if it is a repository name -> id mapping
        let repository_id = hash.to_context();
        {
            let (key, _) = repository::mutable_key(
                repository::SALT_URC,
                repository::METADATA,
                repository_id.into(),
            );
            if let Ok(_value) =
                migrate_lookup(mutable_store.clone(), Partition::default(), key).await
            {
                // The metadata existed, so initial mutable key was a repository name -> id
                lore_debug!(
                    "Mutable key {} found to be repository name -> ID: {}",
                    entry.key,
                    branch_id
                );
                set_key_type(&mut entry.key, KeyType::RepositoryId);
                return Ok(());
            }
        }
    }

    lore_debug!(
        "Mutable key {} found to be non-immutable untyped: {}",
        entry.key,
        entry.value
    );
    set_key_type(&mut entry.key, KeyType::Untyped);
    Ok(())
}

async fn migrate_immutable_value_to_typed(
    entry: &mut MutableStoreEntry,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<MutableStore>,
    remote: &Arc<Connection>,
) -> Result<(), MutableStoreError> {
    let repository = Arc::new(RepositoryContext::new(
        None,
        immutable_store.clone(),
        mutable_store as Arc<dyn store::MutableStore>,
        entry.partition,
        crate::instance::InstanceId::default(),
        Ok(remote.clone()),
        Arc::default(),
        crate::repository::RepositoryFormat::Lore,
    ));
    let hash = entry.value;

    if let Ok(metadata) = metadata::Metadata::deserialize(repository.clone(), hash).await {
        // Check if it is a branch metadata
        if metadata.get_binary(branch::STACK).is_ok()
            || metadata.get_context(branch::PARENT_DEPRECATED).is_ok()
            || metadata
                .get_hash(crate::branch::BRANCH_POINT_DEPRECATED)
                .is_ok()
            || metadata.get_string(branch::CATEGORY).is_ok()
            || metadata.get_bool(branch::PROTECT).is_ok()
        {
            lore_debug!("Mutable key {} found to be branch metadata", entry.key);
            set_key_type(&mut entry.key, KeyType::BranchMetadata);
            return Ok(());
        }

        // Check if it is a repository metadata
        if metadata.get_context(repository::DEFAULT_BRANCH).is_ok()
            || metadata.get_string(repository::DESCRIPTION).is_ok()
        {
            lore_debug!("Mutable key {} found to be repository metadata", entry.key);
            set_key_type(&mut entry.key, KeyType::RepositoryMetadata);
            return Ok(());
        }
    }

    if let Ok(state) = crate::state::State::deserialize(repository, hash).await
        && !state.revision().is_zero()
    {
        // Branch latest pointer
        lore_debug!(
            "Mutable key {} found to be branch latest pointer",
            entry.key
        );
        set_key_type(&mut entry.key, KeyType::BranchLatestPointer);
        return Ok(());
    }

    lore_debug!(
        "Mutable key {} found to be immutable untyped: {}",
        entry.key,
        entry.value
    );
    set_key_type(&mut entry.key, KeyType::Untyped);
    Ok(())
}

async fn migrate_lookup(
    store: Arc<MutableStore>,
    partition: Partition,
    key: Hash,
) -> Result<Hash, StoreError> {
    // Fallback read of old untyped key
    let group_index = key.data()[0];
    let bucket_index = key.data()[1];

    let group = store.group[group_index as usize].clone();
    let bucket = group.bucket(bucket_index as usize).clone();

    let bucket = bucket.read().await;

    let (value, match_made, _) = bucket.lookup(partition, key);
    if match_made && !value.is_zero() {
        Ok(value)
    } else {
        Err(StoreError::from(AddressNotFound::from(
            Address::zero_context_hash(key),
        )))
    }
}
