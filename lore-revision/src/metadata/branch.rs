// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use lore_error_set::prelude::*;

use crate::branch;
use crate::errors::AddressNotFound;
use crate::errors::Disconnected;
use crate::errors::InvalidArguments;
use crate::errors::InvalidPath;
use crate::errors::LinkNotFound;
use crate::errors::NodeNotFound;
use crate::errors::NotFound;
use crate::errors::Oversized;
use crate::errors::PayloadNotFound;
use crate::errors::WriteRequired;
use crate::event;
use crate::event::EventError;
use crate::immutable;
use crate::interface::LoreError;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::repository::RepositoryContext;
use crate::store;
use crate::util::path::RelativePath;

/// Keys that cannot be modified or removed via the branch metadata API.
pub const READ_ONLY_KEYS: &[&str] = &[
    branch::ID,
    branch::NAME,
    branch::CATEGORY,
    branch::CREATOR,
    branch::CREATED,
    branch::STACK,
    branch::PARENT_DEPRECATED,
    branch::BRANCH_POINT_DEPRECATED,
];

/// All built-in keys. These cannot be removed via clear, but `protect` can be overwritten
/// via set.
pub const BUILT_IN_KEYS: &[&str] = &[
    branch::ID,
    branch::NAME,
    branch::CATEGORY,
    branch::CREATOR,
    branch::CREATED,
    branch::PROTECT,
    branch::STACK,
    branch::PARENT_DEPRECATED,
    branch::BRANCH_POINT_DEPRECATED,
];

#[error_set]
pub enum BranchMetadataError {
    InvalidArguments,
    Disconnected,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
}

impl EventError for BranchMetadataError {
    fn translated(&self) -> LoreError {
        match self {
            BranchMetadataError::Disconnected(_) => LoreError::Connection,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn is_read_only_key(key: &str) -> bool {
    READ_ONLY_KEYS.contains(&key)
}

fn is_built_in_key(key: &str) -> bool {
    BUILT_IN_KEYS.contains(&key)
}

fn validate_key(key: &str) -> Result<(), BranchMetadataError> {
    if key.is_empty() {
        return Err(InvalidArguments {
            reason: "metadata key cannot be empty".into(),
        }
        .into());
    }
    Ok(())
}

/// Fetch the current branch metadata hash pointer.
///
/// When `local` is false, queries the remote for the authoritative hash via the
/// `BranchMetadataGet` RPC and caches it locally. Falls back to the local cache if the
/// remote is unavailable. When `local` is true, reads directly from the local mutable store.
async fn fetch_metadata_hash(
    repo: Arc<RepositoryContext>,
    branch: BranchId,
    local: bool,
) -> Result<Hash, BranchMetadataError> {
    if !local
        && let Ok(remote) = repo.remote().await
        && let Ok(revision_service) = remote.revision(repo.id).await
        && let Ok(hash) = revision_service.branch_metadata_get(branch).await
    {
        let _ = branch::mutable_store_metadata(repo.clone(), branch, hash).await;
        return Ok(hash);
    }

    Ok(branch::metadata_hash(repo, branch)
        .await
        .internal("loading branch metadata hash")?)
}

/// Collect all addresses referenced by a metadata blob: the blob itself plus any binary
/// (Address-typed) values it contains.
fn collect_metadata_addresses(metadata: &Metadata, metadata_hash: Hash) -> Vec<Address> {
    let mut addresses = vec![Address {
        hash: metadata_hash,
        context: Context::default(),
    }];

    let _ = metadata.walk(|_key: &[u8], value: &[u8], value_type: MetadataType| {
        if value_type == MetadataType::Address && value.len() == std::mem::size_of::<Address>() {
            addresses.push(value.into());
        }
    });

    addresses
}

/// Query the remote server for which addresses exist, then upload any missing ones from
/// the local immutable store.
async fn ensure_remote_blobs(
    repo: Arc<RepositoryContext>,
    storage: Arc<lore_transport::StorageSession>,
    addresses: &[Address],
) -> Result<(), BranchMetadataError> {
    if addresses.is_empty() {
        return Ok(());
    }

    let status = storage
        .query(addresses)
        .await
        .internal("querying server for metadata blob existence")?;

    let mut missing = vec![];
    for (index, value) in status.iter().enumerate() {
        if *value != 0 && index < addresses.len() {
            missing.push(addresses[index]);
        }
    }

    for address in missing {
        let (fragment, payload) = immutable::load_raw_store_retry(
            repo.immutable_store(),
            repo.id,
            address,
            store::StoreMatch::MatchFull,
        )
        .await
        .internal("loading metadata blob from local store for upload")?;

        immutable::store_raw_remote_retry(storage.clone(), address, fragment, Some(payload))
            .await
            .internal("uploading metadata blob to server")?;
    }

    Ok(())
}

/// Commit an updated metadata hash pointer to the remote via compare-and-swap.
///
/// Before calling the CAS RPC, queries the server to verify all referenced blobs (the
/// metadata blob itself plus any binary Address values) exist on the server, uploading
/// any that are missing from the local store. Then calls `BranchMetadataSet` which
/// validates read-only field protection and binary blob existence before performing the CAS.
/// On success, updates the local cache.
async fn commit_metadata_hash(
    repo: Arc<RepositoryContext>,
    branch: BranchId,
    metadata: &Metadata,
    expected: Hash,
    new: Hash,
) -> Result<(), BranchMetadataError> {
    let remote = repo.remote().await.internal("remote connection required")?;

    let correlation_id = crate::lore::execution_context()
        .globals()
        .correlation_id
        .to_string();
    let storage = remote
        .session(repo.id, &correlation_id)
        .await
        .internal("connecting to storage service")?;

    let addresses = collect_metadata_addresses(metadata, new);
    ensure_remote_blobs(repo.clone(), storage, &addresses).await?;

    let revision_service = remote
        .revision(repo.id)
        .await
        .internal("connecting to revision service")?;
    let result = revision_service
        .branch_metadata_set(branch, expected, new)
        .await
        .internal("branch metadata CAS")?;

    if !result.success {
        return Err(BranchMetadataError::internal(
            "branch metadata was modified concurrently",
        ));
    }

    let _ = branch::mutable_store_metadata(repo, branch, new).await;
    Ok(())
}

/// Retrieve branch metadata. If `key` is provided, emits only that key's value. If `key`
/// is `None`, emits all metadata entries.
///
/// When `local` is true, reads from the local mutable store cache without contacting the remote.
pub async fn get(
    repo: Arc<RepositoryContext>,
    branch: BranchId,
    key: Option<&str>,
    local: bool,
) -> Result<(), BranchMetadataError> {
    let hash = fetch_metadata_hash(repo.clone(), branch, local).await?;
    if hash.is_zero() {
        return Ok(());
    }

    let metadata = Metadata::deserialize(repo, hash)
        .await
        .internal("deserializing branch metadata")?;

    if let Some(key) = key {
        event::metadata::send_keyed(&metadata, key).internal("sending metadata event")?;
    } else {
        event::metadata::send(&metadata).internal("sending metadata event")?;
    }

    Ok(())
}

/// Set one or more metadata key-value pairs on the branch metadata. Always contacts the remote.
///
/// `keys`, `values`, and `formats` must be parallel slices of equal length. For binary values,
/// the value is treated as a file path whose contents are stored in the immutable store.
pub async fn set(
    repo: Arc<RepositoryContext>,
    branch: BranchId,
    keys: &[&[u8]],
    values: &[&[u8]],
    formats: &[MetadataType],
) -> Result<(), BranchMetadataError> {
    if keys.len() != values.len() || keys.len() != formats.len() {
        return Err(InvalidArguments {
            reason: format!(
                "metadata requires matching keys, values, and formats (got {} keys, {} values, {} formats)",
                keys.len(),
                values.len(),
                formats.len()
            ),
        }
        .into());
    }

    for key_bytes in keys.iter() {
        let key = std::str::from_utf8(key_bytes).internal("invalid key encoding")?;
        validate_key(key)?;
        if is_read_only_key(key) {
            return Err(InvalidArguments {
                reason: format!("cannot set read-only key '{key}'"),
            }
            .into());
        }
    }

    let old_hash = fetch_metadata_hash(repo.clone(), branch, false).await?;
    let mut metadata = if old_hash.is_zero() {
        Metadata::new()
    } else {
        Metadata::deserialize(repo.clone(), old_hash)
            .await
            .internal("deserializing branch metadata")?
    };

    for i in 0..keys.len() {
        let key = keys[i];
        let value = values[i];
        let format = formats[i];

        if format == MetadataType::Binary {
            let payload = {
                let user_path = String::from_utf8_lossy(value).to_string();
                let given_path = PathBuf::from(&user_path);
                let input_path = if given_path.is_absolute() {
                    given_path
                } else {
                    let repo_path = repo.require_path()?;
                    let relative_path = RelativePath::new_from_user_path(repo_path, &user_path)
                        .internal("resolving binary metadata path")?;
                    relative_path.to_absolute_path(repo_path)
                };

                tokio::fs::read(input_path)
                    .await
                    .internal("reading binary metadata file")?
            };

            let (address, _) = immutable::write(
                repo.clone(),
                Context::default(),
                Bytes::from_owner(payload),
                immutable::write_options_from_repository(repo.clone()),
            )
            .await
            .internal("writing binary metadata to immutable store")?;

            metadata
                .set_address(
                    std::str::from_utf8(key).internal("invalid key encoding")?,
                    address,
                )
                .internal("setting binary metadata")?;
        } else {
            metadata
                .set(key, value, format)
                .internal("setting metadata")?;
        }
    }

    let new_hash = metadata
        .serialize(repo.clone())
        .await
        .internal("serializing branch metadata")?;

    commit_metadata_hash(repo, branch, &metadata, old_hash, new_hash).await?;

    let _ = event::metadata::send(&metadata);

    Ok(())
}

/// Remove metadata keys from the branch metadata. Always contacts the remote.
///
/// If `keys` is non-empty, removes only those keys (rejecting built-in keys). If `keys` is
/// empty, removes all non-built-in keys.
pub async fn clear(
    repo: Arc<RepositoryContext>,
    branch: BranchId,
    keys: &[&str],
) -> Result<(), BranchMetadataError> {
    for key in keys.iter() {
        if is_built_in_key(key) {
            return Err(InvalidArguments {
                reason: format!("cannot clear built-in key '{key}'"),
            }
            .into());
        }
    }

    let old_hash = fetch_metadata_hash(repo.clone(), branch, false).await?;
    if old_hash.is_zero() {
        return Ok(());
    }

    let mut metadata = Metadata::deserialize(repo.clone(), old_hash)
        .await
        .internal("deserializing branch metadata")?;

    if keys.is_empty() {
        let mut to_remove = vec![];
        metadata
            .walk(
                |key_slice: &[u8], _value_slice: &[u8], _value_type: MetadataType| {
                    if let Ok(key) = std::str::from_utf8(key_slice)
                        && !is_built_in_key(key)
                    {
                        to_remove.push(key.to_string());
                    }
                },
            )
            .internal("walking metadata for clear")?;
        for key in &to_remove {
            metadata.remove_key(key);
        }
    } else {
        for key in keys {
            metadata.remove_key(key);
        }
    }

    let new_hash = metadata
        .serialize(repo.clone())
        .await
        .internal("serializing branch metadata")?;

    commit_metadata_hash(repo, branch, &metadata, old_hash, new_hash).await?;

    Ok(())
}
