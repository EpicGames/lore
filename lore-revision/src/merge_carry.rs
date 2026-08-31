// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//
//! Carry blob for forwarding dirty-only tracking across a `branch merge`.
//!
//! `merge start` refuses to run when the staged state holds actually-staged
//! nodes, but tolerates dirty-only tracking. To preserve that tracking the
//! caller snapshots the dirty paths and stashes them — together with the two
//! revisions about to be merged — under a dedicated mutable-store key. The
//! merge then proceeds with a pruned staged state (so its merkle tree is
//! clean, same invariant as `commit`).
//!
//! Whenever a commit completes, [`take_matching`] consults the carry. If the
//! revisions stored in the blob match the committed state's parents, the
//! paths are returned to be replayed via `file::dirty::dirty_relative_paths`
//! against the freshly committed revision — rebuilding the dirty tracking on
//! top of the merge. The carry is always cleared on commit; `merge abort`
//! also clears it. `merge restart` does nothing — the carry persists across
//! restarts because the merge parents haven't changed.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_storage::store_types::KeyType;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::errors::UnhandledError;
use crate::hash::hash_function;
use crate::immutable;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::util::path::RelativePath;

/// Mutable-store key tag for the carry. The repository id is already the
/// partition the mutable store reads/writes against, so it does not need
/// to be part of the key derivation — the carry is per-repository.
const MERGE_DIRTY_TRACKING: &str = "merge-dirty-tracking";

/// Magic + version on the immutable blob.
const BLOB_MAGIC: u32 = u32::from_le_bytes(*b"MDTB");
const BLOB_VERSION: u32 = 1;

#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
struct BlobHeader {
    magic: u32,
    version: u32,
    parent_self: Hash,
    parent_other: Hash,
    path_count: u32,
    reserved: u32,
}

/// In-memory representation of the carry blob.
#[derive(Clone, Debug)]
pub struct MergeCarry {
    pub parent_self: Hash,
    pub parent_other: Hash,
    pub paths: Vec<RelativePath>,
}

fn carry_key(repository: &RepositoryContext) -> (Hash, KeyType) {
    let key = hash_function(repository.salt(), MERGE_DIRTY_TRACKING);
    (key, KeyType::Untyped)
}

/// Serialize and write the carry blob; store its content hash at the
/// per-instance carry key in the mutable store.
pub async fn store(
    repository: Arc<RepositoryContext>,
    parent_self: Hash,
    parent_other: Hash,
    paths: &[RelativePath],
) -> Result<(), UnhandledError> {
    let header = BlobHeader {
        magic: BLOB_MAGIC,
        version: BLOB_VERSION,
        parent_self,
        parent_other,
        path_count: paths.len() as u32,
        reserved: 0,
    };

    let path_bytes_total: usize = paths.iter().map(|p| 4 + p.as_str().len()).sum();
    let mut bytes = Vec::with_capacity(size_of::<BlobHeader>() + path_bytes_total);
    bytes.extend_from_slice(header.as_bytes());
    for path in paths {
        let s = path.as_str();
        let len = s.len() as u32;
        bytes.extend_from_slice(len.as_bytes());
        bytes.extend_from_slice(s.as_bytes());
    }

    let address = immutable::write(
        repository.clone(),
        Context::default(),
        Bytes::from(bytes),
        immutable::write_options_from_repository(repository.clone())
            .with_local_cache_priority()
            .with_max_size_chunk(),
    )
    .await
    .forward_any::<UnhandledError>("Failed to write merge dirty-tracking blob")?;

    let (key, key_type) = carry_key(&repository);
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| UnhandledError::internal("Write token required to store merge carry"))?;
    handle
        .store(repository.id, key, address.hash, key_type)
        .await
        .forward_any::<UnhandledError>("Failed to store merge dirty-tracking key")?;

    lore_debug!(
        "Stored merge dirty-tracking carry: {} paths, parents {} / {}",
        paths.len(),
        parent_self,
        parent_other,
    );
    Ok(())
}

/// Load the carry blob, if any. Returns `Ok(None)` when no carry exists or
/// the blob is unreadable / malformed (so a corrupt carry never blocks
/// commits — worst case the dirty tracking is dropped, same outcome as a
/// `merge abort` would produce).
pub async fn load(
    repository: Arc<RepositoryContext>,
) -> Result<Option<MergeCarry>, UnhandledError> {
    let (key, key_type) = carry_key(&repository);
    let blob_hash = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()
        .unwrap_or_default();
    if blob_hash.is_zero() {
        return Ok(None);
    }

    let options = immutable::read_options_from_repository(&repository);
    let Ok(data) = immutable::read(
        repository.clone(),
        Address::zero_context_hash(blob_hash),
        None,
        options,
    )
    .await
    else {
        lore_debug!("Merge dirty-tracking blob {blob_hash} not readable, ignoring");
        return Ok(None);
    };

    let raw = data.as_ref();
    let header_size = size_of::<BlobHeader>();
    if raw.len() < header_size {
        return Ok(None);
    }
    let Ok(header) = BlobHeader::read_from_bytes(&raw[..header_size]) else {
        return Ok(None);
    };
    if header.magic != BLOB_MAGIC || header.version != BLOB_VERSION {
        return Ok(None);
    }

    let mut cursor = header_size;
    let mut paths: Vec<RelativePath> = Vec::with_capacity(header.path_count as usize);
    for _ in 0..header.path_count {
        if cursor + 4 > raw.len() {
            return Ok(None);
        }
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&raw[cursor..cursor + 4]);
        let len = u32::from_le_bytes(len_bytes) as usize;
        cursor += 4;
        if cursor + len > raw.len() {
            return Ok(None);
        }
        let Ok(s) = std::str::from_utf8(&raw[cursor..cursor + len]) else {
            return Ok(None);
        };
        if let Ok(rp) = RelativePath::new_from_initial_path(s) {
            paths.push(rp);
        }
        cursor += len;
    }

    Ok(Some(MergeCarry {
        parent_self: header.parent_self,
        parent_other: header.parent_other,
        paths,
    }))
}

/// Clear the carry blob's mutable-store key.
pub async fn delete(repository: Arc<RepositoryContext>) -> Result<(), UnhandledError> {
    let (key, key_type) = carry_key(&repository);
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| UnhandledError::internal("Write token required to delete merge carry"))?;
    handle
        .store(repository.id, key, Hash::default(), key_type)
        .await
        .forward_any::<UnhandledError>("Failed to clear merge dirty-tracking key")?;
    Ok(())
}

/// Returns the carry paths if and only if the blob's two parent revisions
/// match the given pair (order-insensitive — the merge state may have
/// recorded them in either slot). Always clears the carry afterwards so a
/// stale blob doesn't outlive the commit that completed.
pub async fn take_matching(
    repository: Arc<RepositoryContext>,
    new_parent_self: Hash,
    new_parent_other: Hash,
) -> Result<Option<Vec<RelativePath>>, UnhandledError> {
    let Some(carry) = load(repository.clone()).await? else {
        return Ok(None);
    };
    delete(repository).await?;

    let want = [new_parent_self, new_parent_other];
    let have = [carry.parent_self, carry.parent_other];
    let matches =
        (want[0] == have[0] && want[1] == have[1]) || (want[0] == have[1] && want[1] == have[0]);
    if !matches {
        lore_debug!(
            "Merge dirty-tracking carry parents {} / {} do not match commit parents {} / {} — discarding",
            carry.parent_self,
            carry.parent_other,
            new_parent_self,
            new_parent_other,
        );
        return Ok(None);
    }
    Ok(Some(carry.paths))
}
