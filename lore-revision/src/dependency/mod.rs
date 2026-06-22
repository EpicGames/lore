// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod add;
pub mod list;
pub mod remove;
pub mod resolve;

use std::sync::Arc;

pub use add::resolve_path;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event::EventError;
use crate::immutable;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore::Context;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;

/// Metadata key for forward dependencies (what this file depends on).
pub const DEPENDENCIES_KEY: &str = "dependencies";

/// Metadata key for back-references (what depends on this file).
pub const DEPENDENTS_KEY: &str = "dependents";

/// Dependency blobs at or below this size are stored directly in the
/// metadata buffer. Above this size, they are written to the immutable
/// store and an [`Address`] is stored instead.
pub const DEPENDENCY_INLINE_THRESHOLD: usize = 8192;

/// Absolute upper bound for a dependency blob loaded from the immutable
/// store. Caps how large a response the deserializer will materialize so a
/// hostile or corrupt blob can't trigger a multi-gigabyte allocation.
pub const DEPENDENCY_BLOB_MAX_SIZE: usize = 64 * 1024 * 1024;

const MAGIC: u32 = 0x66646570; // "fdep"
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 16; // magic(4) + version(4) + entry_count(4) + reserved(4)

#[error_set]
pub enum DependencyError {
    FileNotFound,
    InvalidArguments,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    RevisionNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for DependencyError {
    fn translated(&self) -> crate::interface::LoreError {
        match self {
            Self::FileNotFound(_) => crate::interface::LoreError::NotFound,
            Self::Disconnected(_) => crate::interface::LoreError::Connection,
            _ => crate::interface::LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

// --- Event data structs ---

/// Event data reported at the start of adding file dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyAddBeginEventData {
    /// Number of source files being processed.
    pub path_count: u64,
    /// Number of dependency edges being added.
    pub dependency_count: u64,
}

/// Event data reported for each dependency edge being added.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyAddEntryEventData {
    /// Path of the source file that gains the dependency.
    pub path: LoreString,
    /// Path of the file being depended on.
    pub dependency: LoreString,
    /// Tags applied to this dependency edge.
    pub tags: LoreArray<LoreString>,
}

/// Event data reported at the end of adding file dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyAddEndEventData {
    /// Number of dependency edges that were added.
    pub added_count: u64,
}

/// Event data reported at the start of removing file dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyRemoveBeginEventData {
    /// Number of source files being processed.
    pub path_count: u64,
    /// Number of dependency edges being removed.
    pub dependency_count: u64,
}

/// Event data reported for each dependency edge being removed.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyRemoveEntryEventData {
    /// Path of the source file that loses the dependency.
    pub path: LoreString,
    /// Path of the file that was depended on.
    pub dependency: LoreString,
    /// Tags on the dependency edge being removed.
    pub tags: LoreArray<LoreString>,
}

/// Event data reported at the end of removing file dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyRemoveEndEventData {
    /// Number of dependency edges that were removed.
    pub removed_count: u64,
}

/// Event data reported at the start of listing file dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyListBeginEventData {
    /// Number of files being listed.
    pub file_count: u64,
}

/// Event data reported at the start of listing a single file's dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyListFileEventData {
    /// Path of the file whose dependencies are being listed.
    pub path: LoreString,
    /// Number of dependency entries for this file.
    pub entry_count: u64,
}

/// Event data reported for each dependency entry in a listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyListEntryEventData {
    /// Path of the dependency.
    pub path: LoreString,
    /// Tags on this dependency edge.
    pub tags: LoreArray<LoreString>,
    /// Traversal depth, zero for a direct dependency.
    pub depth: u32,
}

/// Event data reported at the end of listing a single file's dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyListFileEndEventData {
    /// Path of the file whose dependencies were listed.
    pub path: LoreString,
}

/// Event data reported at the end of listing file dependencies.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDependencyListEndEventData {
    /// Total number of dependency entries that were listed.
    pub total_entry_count: u64,
}

/// Event data reported at the start of dependency resolution.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreDependencyResolveBeginEventData {
    /// Number of root files resolution starts from.
    pub root_count: u64,
}

/// Event data reported for each resolved dependency edge.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreDependencyResolveItemEventData {
    /// Path of the file the dependency comes from.
    pub source: LoreString,
    /// Path of the file the dependency points to.
    pub target: LoreString,
    /// Tags on this dependency edge.
    pub tags: LoreArray<LoreString>,
}

/// Event data reported at the end of dependency resolution.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreDependencyResolveEndEventData {
    /// Number of dependency edges that were resolved.
    pub resolved_count: u64,
}

/// A single dependency relationship to another file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyEntry {
    /// Target node ID within the repository.
    pub node: NodeID,
    /// Application-defined tags categorizing this dependency.
    /// Sorted lexicographically for deterministic serialization.
    /// Uses `Box<str>` instead of `String` to save 8 bytes per tag —
    /// tags are immutable after construction so the extra capacity
    /// field of `String` would be wasted.
    pub tags: Vec<Box<str>>,
}

impl DependencyEntry {
    /// Returns `true` if this entry matches the given tag filter.
    /// An empty filter matches all entries. A non-empty filter matches
    /// if the entry has at least one tag present in the filter.
    pub fn matches_tags<T: AsRef<str>>(&self, filter: &[T]) -> bool {
        filter.is_empty()
            || filter
                .iter()
                .any(|t| self.tags.iter().any(|et| et.as_ref() == t.as_ref()))
    }
}

impl PartialOrd for DependencyEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DependencyEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.node.cmp(&other.node)
    }
}

/// Complete dependency data for a file, serialized as a `DependencyBlob`.
///
/// Entries are sorted by node ID for binary search during lookups.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DependencyData {
    pub entries: Vec<DependencyEntry>,
}

impl DependencyData {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add a dependency on `node` with the given `tags`, merging tags if the
    /// node already exists.
    pub fn add(&mut self, node: NodeID, tags: &[&str]) {
        match self.entries.binary_search_by_key(&node, |e| e.node) {
            Ok(idx) => {
                let entry = &mut self.entries[idx];
                for &tag in tags {
                    if let Err(pos) = entry.tags.binary_search_by(|t| t.as_ref().cmp(tag)) {
                        entry.tags.insert(pos, tag.into());
                    }
                }
            }
            Err(idx) => {
                let mut sorted_tags: Vec<Box<str>> = tags.iter().map(|&t| t.into()).collect();
                sorted_tags.sort();
                sorted_tags.dedup();
                self.entries.insert(
                    idx,
                    DependencyEntry {
                        node,
                        tags: sorted_tags,
                    },
                );
            }
        }
    }

    /// Remove a dependency on `node`. If `tags` is non-empty, only those tags
    /// are removed; the entry is removed entirely when no tags remain. If
    /// `tags` is empty, the entire entry is removed regardless of its tags.
    ///
    /// Returns `true` if the entry was fully removed.
    pub fn remove(&mut self, node: NodeID, tags: &[&str]) -> bool {
        let Ok(idx) = self.entries.binary_search_by_key(&node, |e| e.node) else {
            return false;
        };

        if tags.is_empty() {
            self.entries.remove(idx);
            return true;
        }

        let entry = &mut self.entries[idx];
        entry.tags.retain(|t| !tags.contains(&t.as_ref()));

        if entry.tags.is_empty() {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    /// Returns `true` if a dependency on `node` exists.
    pub fn contains(&self, node: NodeID) -> bool {
        self.entries.binary_search_by_key(&node, |e| e.node).is_ok()
    }

    /// Returns `true` if a dependency on `node` exists with the given `tag`.
    pub fn contains_tag(&self, node: NodeID, tag: &str) -> bool {
        match self.entries.binary_search_by_key(&node, |e| e.node) {
            Ok(idx) => self.entries[idx].tags.iter().any(|t| t.as_ref() == tag),
            Err(_) => false,
        }
    }

    /// Returns the entry for `node`, if it exists.
    pub fn get(&self, node: NodeID) -> Option<&DependencyEntry> {
        self.entries
            .binary_search_by_key(&node, |e| e.node)
            .ok()
            .map(|idx| &self.entries[idx])
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DependencyEntry> {
        self.entries.iter()
    }

    /// Serialize to the `DependencyBlob` binary format.
    pub fn serialize(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.serialized_size());

        buf.put_u32_le(MAGIC);
        buf.put_u32_le(VERSION);
        buf.put_u32_le(self.entries.len() as u32);
        buf.put_u32_le(0); // reserved

        for entry in &self.entries {
            buf.put_u32_le(entry.node);
            buf.put_u16_le(entry.tags.len() as u16);
            buf.put_u16_le(0); // reserved

            for tag in &entry.tags {
                let tag_bytes = tag.as_bytes();
                buf.put_u16_le(tag_bytes.len() as u16);
                buf.put_slice(tag_bytes);
                // Pad to 2-byte alignment
                if !tag_bytes.len().is_multiple_of(2) {
                    buf.put_u8(0);
                }
            }
        }

        buf.freeze()
    }

    /// Deserialize from a `DependencyBlob` byte buffer.
    pub fn deserialize(buffer: &[u8]) -> Result<Self, Traced<Internal>> {
        if buffer.len() < HEADER_SIZE {
            return Err(Internal::msg("dependency blob too short").into());
        }

        let magic = u32::from_le_bytes(
            buffer[0..4]
                .try_into()
                .internal("dependency blob too short")?,
        );
        if magic != MAGIC {
            return Err(Internal::msg("dependency blob bad magic").into());
        }

        let version = u32::from_le_bytes(
            buffer[4..8]
                .try_into()
                .internal("dependency blob too short")?,
        );
        if version != VERSION {
            return Err(Internal::msg("dependency blob unsupported version").into());
        }

        let entry_count = u32::from_le_bytes(
            buffer[8..12]
                .try_into()
                .internal("dependency blob too short")?,
        ) as usize;

        // Bound entry_count by what the buffer can physically hold before
        // allocating. Each entry requires at least 8 bytes (node_id + tag_count
        // + reserved); a hostile or corrupt blob that declares a huge count
        // would otherwise trigger a `Vec::with_capacity` allocation in the
        // tens of gigabytes.
        const MIN_ENTRY_BYTES: usize = 8;
        let max_entries = buffer.len().saturating_sub(HEADER_SIZE) / MIN_ENTRY_BYTES;
        if entry_count > max_entries {
            return Err(
                Internal::msg("dependency blob entry_count exceeds what buffer can hold").into(),
            );
        }

        let mut offset = HEADER_SIZE;
        let mut entries = Vec::with_capacity(entry_count);

        for _ in 0..entry_count {
            // Each entry needs at least node_id(4) + tag_count(2) + reserved(2) = 8 bytes
            if offset + 8 > buffer.len() {
                return Err(Internal::msg("dependency blob truncated entry").into());
            }

            let node_id = u32::from_le_bytes(
                buffer[offset..offset + 4]
                    .try_into()
                    .internal("dependency blob truncated entry")?,
            );
            offset += 4;

            let tag_count = u16::from_le_bytes(
                buffer[offset..offset + 2]
                    .try_into()
                    .internal("dependency blob truncated entry")?,
            ) as usize;
            offset += 4; // tag_count(2) + reserved(2)

            // Bound tag_count by what the remaining buffer can physically
            // hold before allocating. Each tag requires at least 2 bytes
            // (the length prefix, even for a zero-byte tag); without this,
            // a small blob that declares tag_count = 65535 forces a ~1 MiB
            // transient Vec allocation per entry.
            if tag_count.saturating_mul(2) > buffer.len().saturating_sub(offset) {
                return Err(
                    Internal::msg("dependency blob tag_count exceeds remaining buffer").into(),
                );
            }

            let mut tags = Vec::with_capacity(tag_count);
            for _ in 0..tag_count {
                if offset + 2 > buffer.len() {
                    return Err(Internal::msg("dependency blob truncated tag").into());
                }

                let tag_length = u16::from_le_bytes(
                    buffer[offset..offset + 2]
                        .try_into()
                        .internal("dependency blob truncated tag")?,
                ) as usize;
                offset += 2;

                if offset + tag_length > buffer.len() {
                    return Err(Internal::msg("dependency blob truncated tag data").into());
                }

                let tag_str = std::str::from_utf8(&buffer[offset..offset + tag_length])
                    .internal("dependency blob invalid UTF-8 in tag")?;
                tags.push(Box::from(tag_str));
                offset += tag_length;

                // Skip padding to 2-byte alignment
                if !tag_length.is_multiple_of(2) {
                    offset += 1;
                    if offset > buffer.len() {
                        return Err(Internal::msg("dependency blob truncated tag padding").into());
                    }
                }
            }

            entries.push(DependencyEntry {
                node: node_id,
                tags,
            });
        }

        Ok(Self { entries })
    }

    /// Returns the exact byte count that [`serialize`](Self::serialize) will
    /// produce.
    fn serialized_size(&self) -> usize {
        let mut size = HEADER_SIZE;
        for entry in &self.entries {
            size += 8; // node_id(4) + tag_count(2) + reserved(2)
            for tag in &entry.tags {
                let tag_len = tag.len();
                // tag_length(2) + tag_data + optional padding byte
                size += 2 + tag_len + (tag_len & 1);
            }
        }
        size
    }
}

/// Load dependency data for a node from its file metadata.
///
/// Reads the `NodeFileMetadata` for the given node, deserializes the
/// `Metadata`, and extracts the dependency blob stored under `key`.
/// Handles both inline (`MetadataType::Binary`) and indirect
/// (`MetadataType::Address`) storage transparently.
///
/// Returns an empty `DependencyData` if the node has no metadata or
/// the key is not present.
pub async fn load_dependency_data(
    repository: Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
    key: &str,
) -> Result<DependencyData, DependencyError> {
    let metadata_node = node::node_to_file_metadata(node_id);
    let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
    let metadata_node_index = NodeFileMetadata::index(metadata_node);

    let metadata_block = state
        .block_file_metadata(repository.clone(), metadata_block_index)
        .await
        .internal("loading metadata block")?;

    let metadata_hash = {
        let block_reader = metadata_block.read();
        block_reader.node(metadata_node_index).metadata
    };

    if metadata_hash.is_zero() {
        return Ok(DependencyData::new());
    }

    let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .internal("deserializing metadata")?;

    let (value_bytes, metadata_type) = match metadata.get_typed(key) {
        Ok(result) => result,
        Err(crate::metadata::MetadataError::FileNotFound(_)) => return Ok(DependencyData::new()),
        Err(err) => {
            return Err(DependencyError::internal_with_context(
                err,
                "reading dependency metadata key",
            ));
        }
    };

    match metadata_type {
        MetadataType::Binary => Ok(DependencyData::deserialize(value_bytes)?),
        MetadataType::Address => {
            let address =
                Metadata::to_address(value_bytes).internal("parsing dependency address")?;
            let options = immutable::read_options_from_repository(&repository)
                .with_cache()
                .with_max_content_size(DEPENDENCY_BLOB_MAX_SIZE as u64);
            let blob = immutable::read(repository, address, None, options)
                .await
                .internal("reading dependency blob from immutable store")?;
            Ok(DependencyData::deserialize(&blob)?)
        }
        _ => Err(Internal::msg("unexpected metadata type for dependency key").into()),
    }
}

/// Store dependency data for a node in its file metadata.
///
/// Uses an optimistic locking loop to handle concurrent metadata
/// modifications. The dependency blob is stored inline
/// (`MetadataType::Binary`) when at or below [`DEPENDENCY_INLINE_THRESHOLD`],
/// or written to the immutable store with an `Address` reference
/// (`MetadataType::Address`) when larger.
///
/// When `data` is empty, the metadata key is removed entirely.
pub async fn store_dependency_data(
    repository: Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
    key: &str,
    data: &DependencyData,
) -> Result<(), DependencyError> {
    // Reject blobs that would exceed the read-side cap before writing. The
    // deserializer caps loads at DEPENDENCY_BLOB_MAX_SIZE, so storing
    // anything larger would be a write we could never read back — fail at
    // the setter rather than trap the data.
    let serialized_size = data.serialized_size();
    if serialized_size > DEPENDENCY_BLOB_MAX_SIZE {
        return Err(DependencyError::from(lore_base::error::InvalidArguments {
            reason: format!(
                "dependency blob for key {key} is {serialized_size} bytes, exceeds DEPENDENCY_BLOB_MAX_SIZE ({DEPENDENCY_BLOB_MAX_SIZE})"
            ),
        }));
    }

    let metadata_node = node::node_to_file_metadata(node_id);
    let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
    let metadata_node_index = NodeFileMetadata::index(metadata_node);

    let metadata_block = state
        .block_file_metadata(repository.clone(), metadata_block_index)
        .await
        .internal("loading metadata block for store")?;

    loop {
        let metadata_hash = {
            let block_reader = metadata_block.read();
            block_reader.node(metadata_node_index).metadata
        };

        let mut metadata = if metadata_hash.is_zero() {
            Metadata::new()
        } else {
            Metadata::deserialize(repository.clone(), metadata_hash)
                .await
                .internal("deserializing metadata for store")?
        };

        if data.is_empty() {
            metadata.remove_key(key);
        } else {
            let blob = data.serialize();
            if blob.len() <= DEPENDENCY_INLINE_THRESHOLD {
                metadata
                    .set_binary(key, &blob)
                    .internal("setting inline dependency blob")?;
            } else {
                let (address, _) = immutable::write(
                    repository.clone(),
                    Context::default(),
                    blob,
                    immutable::write_options_from_repository(repository.clone())
                        .with_local_cache_priority(),
                )
                .await
                .internal("writing dependency blob to immutable store")?;
                metadata
                    .set_address(key, address)
                    .internal("setting dependency address in metadata")?;
            }
        }

        let metadata_hash_updated = metadata
            .serialize(repository.clone())
            .await
            .internal("serializing metadata")?;

        let dirtied = {
            let mut block_writer = metadata_block.write();
            let node = block_writer.node(metadata_node_index);

            if node.metadata != metadata_hash {
                continue;
            }

            if node.metadata != metadata_hash_updated {
                node.metadata = metadata_hash_updated;
                block_writer.mark_dirty()
            } else {
                false
            }
        };

        if dirtied {
            state.block_file_metadata_modified(metadata_block, metadata_block_index);
            state.mark_dirty();
        }

        break;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw dependency blob header (16 bytes) with the given
    /// `entry_count`. Used by tests to craft malformed inputs without going
    /// through the well-formed `serialize` path.
    fn blob_header(entry_count: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE);
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&entry_count.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        buf
    }

    #[test]
    fn deserialize_roundtrip() {
        let mut data = DependencyData::new();
        data.add(7, &["foo", "bar"]);
        data.add(42, &["baz"]);
        let bytes = data.serialize();
        let parsed = DependencyData::deserialize(&bytes).expect("roundtrip");
        assert_eq!(parsed, data);
    }

    #[test]
    fn deserialize_empty_blob_ok() {
        let bytes = DependencyData::new().serialize();
        let parsed = DependencyData::deserialize(&bytes).expect("empty roundtrip");
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn deserialize_rejects_entry_count_exceeding_buffer() {
        // Header declares 1_000_000 entries but buffer only has the header.
        // Without the cap, this triggers a ~32 MiB Vec::with_capacity.
        let bytes = blob_header(1_000_000);
        let err = DependencyData::deserialize(&bytes).expect_err("should reject");
        assert!(
            err.to_string().contains("entry_count exceeds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_u32_max_entry_count() {
        let bytes = blob_header(u32::MAX);
        let err = DependencyData::deserialize(&bytes).expect_err("should reject");
        assert!(
            err.to_string().contains("entry_count exceeds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_tag_count_exceeding_remaining_buffer() {
        // Header says 1 entry. Entry header says tag_count = 65535. Remaining
        // buffer (0 bytes after the entry header) can't fit 65535 * 2 bytes.
        // Without the cap, this triggers a ~1 MiB Vec::with_capacity for tags.
        let mut bytes = blob_header(1);
        bytes.extend_from_slice(&7u32.to_le_bytes()); // node_id
        bytes.extend_from_slice(&u16::MAX.to_le_bytes()); // tag_count
        bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
        let err = DependencyData::deserialize(&bytes).expect_err("should reject");
        assert!(
            err.to_string().contains("tag_count exceeds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_accepts_legitimate_tag_counts() {
        // Confirm the tag_count cap doesn't break well-formed blobs with
        // multiple tags.
        let mut data = DependencyData::new();
        data.add(1, &["a", "b", "c", "d", "e"]);
        let bytes = data.serialize();
        let parsed = DependencyData::deserialize(&bytes).expect("should accept");
        assert_eq!(parsed, data);
    }

    #[test]
    fn deserialize_rejects_bad_magic() {
        let mut bytes = blob_header(0);
        bytes[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert!(DependencyData::deserialize(&bytes).is_err());
    }

    #[test]
    fn deserialize_rejects_unsupported_version() {
        let mut bytes = blob_header(0);
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        assert!(DependencyData::deserialize(&bytes).is_err());
    }

    #[test]
    fn deserialize_rejects_blob_shorter_than_header() {
        let bytes = vec![0u8; HEADER_SIZE - 1];
        assert!(DependencyData::deserialize(&bytes).is_err());
    }
}
