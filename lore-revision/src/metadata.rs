// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod branch;
pub mod clear;
pub mod find;
pub mod get;
pub mod list;
pub mod repository;
pub mod set;

use std::sync::Arc;

use bytes::BytesMut;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_error_set::prelude::*;
use zerocopy::IntoBytes;

use crate::errors::*;
use crate::event::EventError;
use crate::immutable;
use crate::interface::LoreError;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::repository::RepositoryContext;

/// Maximum serialized metadata blob size. Metadata is loaded fully into memory
/// at deserialize time; callers needing to attach larger data should store it
/// as a separate immutable blob and reference it via an [`Address`] or [`Hash`]
/// value in the metadata.
pub const METADATA_MAX_SIZE: usize = 1024 * 1024;

#[error_set]
pub enum MetadataErrors {
    InvalidArguments,
    FileNotFound,
    Oversized,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    WriteRequired,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    RevisionNotFound,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
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

impl EventError for MetadataErrors {
    fn translated(&self) -> LoreError {
        match self {
            Self::FileNotFound(_) => LoreError::NotFound,
            Self::Oversized(_) => LoreError::Oversized,
            Self::Disconnected(_) => LoreError::Connection,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Commit message ([`MetadataType::String`])
pub const MESSAGE: &str = "message";
/// Timestamp when revision was committed (`u64`)
pub const TIMESTAMP: &str = "timestamp";
/// Creator(s) of the revision ([`MetadataType::String`])
pub const CREATED_BY: &str = "created-by";
/// Committer of the revision ([`MetadataType::String`])
pub const COMMITTED_BY: &str = "committed-by";
/// Reviewer(s) of the revision ([`MetadataType::String`])
pub const REVIEWED_BY: &str = "reviewed-by";
/// Merger of the revision ([`MetadataType::String`])
pub const MERGED_BY: &str = "merged-by";
/// Originating branch ID ([`MetadataType::Context`])
pub const BRANCH: &str = "branch";
/// Associated P4 changelist ([`MetadataType::String`])
pub const P4_CHANGELIST: &str = "p4-changelist";
/// Originating restored revision ([`MetadataType::String`])
pub const RESTORED_FROM: &str = "restored-from";
/// Originating cherry-picked revision ([`MetadataType::String`])
pub const CHERRY_PICKED_FROM: &str = "cherry-picked-from";
/// Originating reverted revision ([`MetadataType::String`])
pub const REVERTED_FROM: &str = "reverted-from";
/// Change request ID of the revision ([`MetadataType::String`])
pub const CHANGE_REQUEST: &str = "change-request";
/// Indicates the revision was created by a fast-forward merge ([`MetadataType::Numeric`])
pub const FAST_FORWARD_MERGE: &str = "fast-forward-merge";

#[error_set]
pub enum MetadataError {
    InvalidArguments,
    FileNotFound,
    Oversized,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    WriteRequired,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    SlowDown,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
}

#[derive(Debug)]
pub struct Metadata {
    buffer: BytesMut,
}

/// Type tag for a metadata value.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MetadataType {
    /// Value is an address.
    Address = 1,
    /// Value is a boolean.
    Boolean = 2,
    /// Value is a context.
    Context = 3,
    /// Value is a hash.
    Hash = 4,
    /// Value is an unsigned integer.
    Numeric = 5,
    /// Value is text.
    String = 6,
    /// Value is raw binary data.
    Binary = 255,
}

impl MetadataType {
    pub fn from(num: u32) -> Self {
        match num {
            1 => Self::Address,
            2 => Self::Boolean,
            3 => Self::Context,
            4 => Self::Hash,
            5 => Self::Numeric,
            6 => Self::String,
            _ => Self::Binary,
        }
    }
}

/// Header at the start of a serialized metadata blob.
#[repr(C)]
pub struct MetadataHeader {
    /// Identifier marking the buffer as metadata.
    pub magic: u32,
    /// Format version of the metadata layout.
    pub version: u32,
}

const MAGIC: u32 = 0x6D657461; // 'meta'
const VERSION: u32 = 1;

/// Metadata item
#[repr(C)]
struct MetadataItem {
    /// Length of key data
    key_length: u32,
    /// Length of value data
    value_length: u32,
    /// Type of value data
    value_type: u32,
    // Followed by the key data, then the value data
}

static DEFAULT_METADATA_CAPACITY: usize = if FRAGMENT_SIZE_THRESHOLD > 64 * 1024 {
    64 * 1024
} else {
    FRAGMENT_SIZE_THRESHOLD
};

impl Clone for Metadata {
    fn clone(&self) -> Self {
        Self::new_with_buffer(self.buffer.clone())
    }

    fn clone_from(&mut self, source: &Self) {
        self.buffer = source.buffer.clone();
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self::new()
    }
}

impl Metadata {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(DEFAULT_METADATA_CAPACITY),
        }
    }

    fn new_with_buffer(buffer: BytesMut) -> Self {
        Self { buffer }
    }

    pub async fn deserialize(
        repository: Arc<RepositoryContext>,
        hash: Hash,
    ) -> Result<Self, MetadataError> {
        let address = Address {
            hash,
            context: Context::default(),
        };
        let options = immutable::read_options_from_repository(&repository)
            .with_cache()
            .with_max_content_size(METADATA_MAX_SIZE as u64);
        let buffer = immutable::read(
            repository, address, None, /* No range, read all */
            options,
        )
        .await
        .forward::<MetadataError>("reading metadata")?;

        let metadata = Self::new_with_buffer(BytesMut::from(buffer));
        metadata.check_header()?;

        Ok(metadata)
    }

    pub async fn serialize(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Hash, MetadataError> {
        self.serialize_with_tracker(repository, None).await
    }

    /// Tracker-aware variant of [`serialize`]: routes the metadata write
    /// through the supplied [`WriteTracker`] so a commit can await the
    /// background upload before finalising its branch pointer.
    pub async fn serialize_with_tracker(
        &self,
        repository: Arc<RepositoryContext>,
        tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
    ) -> Result<Hash, MetadataError> {
        if self.is_empty() {
            return Ok(Hash::default());
        }

        if self.buffer.len() > METADATA_MAX_SIZE {
            return Err(MetadataError::from(Oversized {
                context: format!(
                    "metadata size {} exceeds {METADATA_MAX_SIZE} byte limit; store \
                     large values as separate blobs and reference them via hash",
                    self.buffer.len()
                ),
            }));
        }

        let buffer = self.buffer.clone();
        let (address, _) = immutable::write_with_tracker(
            repository.clone(),
            Context::default(),
            buffer.freeze(),
            immutable::write_options_from_repository(repository.clone())
                .with_local_cache_priority(),
            tracker,
        )
        .await
        .forward::<MetadataError>("writing metadata")?;

        Ok(address.hash)
    }

    /// Serialize metadata to the local immutable store only (never uploaded to remote).
    pub async fn serialize_local(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Hash, MetadataError> {
        if self.is_empty() {
            return Ok(Hash::default());
        }

        if self.buffer.len() > METADATA_MAX_SIZE {
            return Err(MetadataError::from(Oversized {
                context: format!(
                    "metadata size {} exceeds {METADATA_MAX_SIZE} byte limit; store \
                     large values as separate blobs and reference them via hash",
                    self.buffer.len()
                ),
            }));
        }

        let buffer = self.buffer.clone();
        let (address, _) = immutable::write(
            repository.clone(),
            Context::default(),
            buffer.freeze(),
            lore_storage::WriteOptions::default()
                .with_local_cache_priority()
                .no_remote_write(),
        )
        .await
        .forward::<MetadataError>("writing metadata (local)")?;

        Ok(address.hash)
    }

    pub fn set_branch(&mut self, branch: BranchId) -> Result<(), MetadataError> {
        self.set_context(BRANCH, branch)
    }

    pub fn get_branch(&self) -> Result<BranchId, MetadataError> {
        self.get_context(BRANCH)
    }

    pub fn set_timestamp(&mut self, timestamp: u64) -> Result<(), MetadataError> {
        self.set_u64(TIMESTAMP, timestamp)
    }

    pub fn get_timestamp(&self) -> Result<u64, MetadataError> {
        self.get_u64(TIMESTAMP)
    }

    pub fn get_string<'a>(&'a self, key: &str) -> Result<&'a str, MetadataError> {
        Self::to_string(self.get(key.as_bytes())?)
    }

    pub fn get_context(&self, key: &str) -> Result<Context, MetadataError> {
        Self::to_context(self.get(key.as_bytes())?)
    }

    pub fn get_hash(&self, key: &str) -> Result<Hash, MetadataError> {
        Self::to_hash(self.get(key.as_bytes())?)
    }

    pub fn get_address(&self, key: &str) -> Result<Address, MetadataError> {
        Self::to_address(self.get(key.as_bytes())?)
    }

    pub fn get_u64(&self, key: &str) -> Result<u64, MetadataError> {
        Self::to_u64(self.get(key.as_bytes())?)
    }

    pub fn get_bool(&self, key: &str) -> Result<bool, MetadataError> {
        Self::to_bool(self.get(key.as_bytes())?)
    }

    pub fn get_binary(&self, key: &str) -> Result<&[u8], MetadataError> {
        self.get(key.as_bytes())
    }

    /// Returns the raw value bytes and the [`MetadataType`] for the given key.
    pub fn get_typed(&self, key: &str) -> Result<(&[u8], MetadataType), MetadataError> {
        self.get_with_type(key.as_bytes())
    }

    pub fn to_string(value: &[u8]) -> Result<&str, MetadataError> {
        Ok(std::str::from_utf8(value).internal("metadata type mismatch")?)
    }

    pub fn to_context(value: &[u8]) -> Result<Context, MetadataError> {
        if value.len() == std::mem::size_of::<Context>() {
            Ok(value.into())
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    pub fn to_hash(value: &[u8]) -> Result<Hash, MetadataError> {
        if value.len() == std::mem::size_of::<Hash>() {
            Ok(value.into())
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    pub fn to_address(value: &[u8]) -> Result<Address, MetadataError> {
        if value.len() == std::mem::size_of::<Address>() {
            Ok(value.into())
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    pub fn to_u64(value: &[u8]) -> Result<u64, MetadataError> {
        if value.len() == std::mem::size_of::<u64>() {
            Ok(u64::from_le_bytes(
                value.try_into().internal("metadata type mismatch")?,
            ))
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    /// Decodes a string representation of a metadata value into its byte encoding
    /// based on the metadata type. Numeric values are parsed as `u64` and stored as
    /// little-endian bytes; all other types are passed through as raw UTF-8 bytes.
    pub fn decode_to_value(value: &str, format: &MetadataType) -> Result<Vec<u8>, MetadataError> {
        match format {
            MetadataType::Numeric => {
                let parsed: u64 = value
                    .parse()
                    .map_err(|_parse_err| MetadataError::internal("invalid numeric value"))?;
                Ok(parsed.to_le_bytes().to_vec())
            }
            _ => Ok(value.as_bytes().to_vec()),
        }
    }

    pub fn to_bool(value: &[u8]) -> Result<bool, MetadataError> {
        if value.len() == 1 {
            Ok(value[0] != 0)
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    fn get<'a>(&'a self, key: &[u8]) -> Result<&'a [u8], MetadataError> {
        if self.is_empty() {
            return Err(FileNotFound {
                resource: "metadata key".into(),
            }
            .into());
        }

        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        let mut offset = header_size;
        let buffer = self.buffer.as_ref();
        while offset < self.buffer.len() {
            let raw_pointer = unsafe { buffer.as_ptr().add(offset).cast::<MetadataItem>() };
            let item: MetadataItem = unsafe { raw_pointer.read_unaligned() };

            let key_length = item.key_length as usize;
            let value_length = item.value_length as usize;
            offset += item_size;

            let key_data = unsafe { buffer.as_ptr().add(offset) };
            let key_slice = unsafe { std::slice::from_raw_parts(key_data, key_length) };
            offset += key_length;

            if key_slice == key {
                let value_data = unsafe { buffer.as_ptr().add(offset) };
                return Ok(unsafe { std::slice::from_raw_parts(value_data, value_length) });
            }

            offset += value_length;
        }
        Err(FileNotFound {
            resource: "metadata key".into(),
        }
        .into())
    }

    fn get_with_type<'a>(&'a self, key: &[u8]) -> Result<(&'a [u8], MetadataType), MetadataError> {
        if self.is_empty() {
            return Err(FileNotFound {
                resource: "metadata key".into(),
            }
            .into());
        }

        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        let mut offset = header_size;
        let buffer = self.buffer.as_ref();
        while offset < self.buffer.len() {
            let raw_pointer = unsafe { buffer.as_ptr().add(offset).cast::<MetadataItem>() };
            let item: MetadataItem = unsafe { raw_pointer.read_unaligned() };

            let key_length = item.key_length as usize;
            let value_length = item.value_length as usize;
            let value_type = item.value_type;
            offset += item_size;

            let key_data = unsafe { buffer.as_ptr().add(offset) };
            let key_slice = unsafe { std::slice::from_raw_parts(key_data, key_length) };
            offset += key_length;

            if key_slice == key {
                let value_data = unsafe { buffer.as_ptr().add(offset) };
                let value_slice = unsafe { std::slice::from_raw_parts(value_data, value_length) };
                return Ok((value_slice, MetadataType::from(value_type)));
            }

            offset += value_length;
        }
        Err(FileNotFound {
            resource: "metadata key".into(),
        }
        .into())
    }

    pub fn set_string(&mut self, key: &str, value: &str) -> Result<(), MetadataError> {
        self.set(key.as_bytes(), value.as_bytes(), MetadataType::String)
    }

    pub fn set_u64(&mut self, key: &str, value: u64) -> Result<(), MetadataError> {
        self.set(
            key.as_bytes(),
            value.to_le_bytes().as_slice(),
            MetadataType::Numeric,
        )
    }

    pub fn set_context(&mut self, key: &str, value: Context) -> Result<(), MetadataError> {
        self.set(key.as_bytes(), value.as_bytes(), MetadataType::Context)
    }

    pub fn set_hash(&mut self, key: &str, value: Hash) -> Result<(), MetadataError> {
        self.set(key.as_bytes(), value.as_bytes(), MetadataType::Hash)
    }

    pub fn set_address(&mut self, key: &str, value: Address) -> Result<(), MetadataError> {
        self.set(key.as_bytes(), value.as_bytes(), MetadataType::Address)
    }

    pub fn set_bool(&mut self, key: &str, value: bool) -> Result<(), MetadataError> {
        self.set(
            key.as_bytes(),
            if value { &[1u8] } else { &[0u8] },
            MetadataType::Boolean,
        )
    }

    pub fn set_binary(&mut self, key: &str, value: &[u8]) -> Result<(), MetadataError> {
        self.set(key.as_bytes(), value, MetadataType::Binary)
    }

    /// Remove a key from the metadata. Returns `true` if the key existed.
    pub fn remove_key(&mut self, key: &str) -> bool {
        self.remove(key.as_bytes())
    }

    fn remove(&mut self, key: &[u8]) -> bool {
        if self.is_empty() {
            return false;
        }

        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        let mut offset = header_size;
        while offset < self.buffer.len() {
            let buffer = self.buffer.as_mut();
            let raw_pointer = unsafe { buffer.as_ptr().add(offset).cast::<MetadataItem>() };
            let item: MetadataItem = unsafe { raw_pointer.read_unaligned() };

            let key_length = item.key_length as usize;
            let value_length = item.value_length as usize;
            let start_offset = offset;
            offset += item_size;

            let key_data = unsafe { buffer.as_ptr().add(offset) };
            let key_slice = unsafe { std::slice::from_raw_parts(key_data, key_length) };
            offset += key_length;

            if key == key_slice {
                let block_size = item_size + key_length + value_length;
                let next_offset = start_offset + block_size;
                if buffer.len() > next_offset {
                    unsafe {
                        std::ptr::copy(
                            buffer.as_mut_ptr().add(next_offset),
                            buffer.as_mut_ptr().add(start_offset),
                            self.buffer.len() - next_offset,
                        );
                    }
                    self.buffer.truncate(self.buffer.len() - block_size);
                } else {
                    self.buffer.truncate(start_offset);
                }
                return true;
            }

            offset += value_length;
        }
        false
    }

    fn set(
        &mut self,
        key: &[u8],
        value: &[u8],
        value_type: MetadataType,
    ) -> Result<(), MetadataError> {
        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        self.set_header()?;

        let mut offset = header_size;
        while offset < self.buffer.len() {
            let buffer = self.buffer.as_mut();
            let raw_pointer = unsafe { buffer.as_ptr().add(offset).cast::<MetadataItem>() };
            let item: MetadataItem = unsafe { raw_pointer.read_unaligned() };

            let key_length = item.key_length as usize;
            let value_length = item.value_length as usize;
            let start_offset = offset;
            offset += item_size;

            let key_data = unsafe { buffer.as_ptr().add(offset) };
            let key_slice = unsafe { std::slice::from_raw_parts(key_data, key_length) };
            offset += key_length;

            if key == key_slice {
                if value_length == value.len() {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            value.as_ptr(),
                            buffer.as_mut_ptr().add(offset),
                            value.len(),
                        );
                    }
                    return Ok(());
                }

                // Erase the current key-value pair by moving remaining items
                let block_size = item_size + key_length + value_length;
                let next_offset = start_offset + block_size;
                if buffer.len() > next_offset {
                    unsafe {
                        std::ptr::copy(
                            buffer.as_mut_ptr().add(next_offset),
                            buffer.as_mut_ptr().add(start_offset),
                            self.buffer.len() - next_offset,
                        );
                    }
                    self.buffer.truncate(self.buffer.len() - block_size);
                } else {
                    self.buffer.truncate(start_offset);
                }

                break;
            }

            offset += value_length;
        }

        let block_size = item_size + key.len() + value.len();
        let current_size = self.buffer.len();
        let next_size = current_size + block_size;
        if next_size > self.buffer.capacity() {
            // reserve takes "additional bytes to insert", so always pass
            // the block size (or a minimum 4KiB slab) regardless of the
            // current slack between len and capacity.
            self.buffer.reserve(std::cmp::max(block_size, 4000));
        }

        let item = MetadataItem {
            key_length: key.len() as u32,
            value_length: value.len() as u32,
            value_type: value_type as u32,
        };
        let item_pointer = unsafe {
            self.buffer
                .as_mut_ptr()
                .add(current_size)
                .cast::<MetadataItem>()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::addr_of!(item).cast::<u8>(),
                item_pointer.cast::<u8>(),
                item_size,
            );
        }
        offset = current_size + item_size;

        unsafe {
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                self.buffer.as_mut_ptr().add(offset),
                key.len(),
            );
        }
        offset += key.len();

        unsafe {
            std::ptr::copy_nonoverlapping(
                value.as_ptr(),
                self.buffer.as_mut_ptr().add(offset),
                value.len(),
            );
        }

        unsafe { self.buffer.set_len(next_size) };

        Ok(())
    }

    fn set_header(&mut self) -> Result<(), MetadataError> {
        if self.is_empty() {
            self.buffer.reserve(4000);

            let header_size = std::mem::size_of::<u32>() * 2;
            let header = MetadataHeader {
                magic: MAGIC,
                version: VERSION,
            };
            let header_pointer = self.buffer.as_mut_ptr().cast::<MetadataHeader>();

            unsafe {
                std::ptr::copy_nonoverlapping(
                    std::ptr::addr_of!(header).cast::<u8>(),
                    header_pointer.cast::<u8>(),
                    header_size,
                );
            }

            unsafe { self.buffer.set_len(header_size) };
        }

        Ok(())
    }

    fn check_header(&self) -> Result<(), MetadataError> {
        let buffer_size = self.buffer.len();
        if buffer_size > 0 {
            let header_size = std::mem::size_of::<u32>() * 2;
            if header_size > buffer_size {
                return Err(MetadataError::internal("bad metadata header"));
            }

            let buffer = self.buffer.as_ref();
            let raw_pointer = buffer.as_ptr().cast::<MetadataHeader>();
            let header: MetadataHeader = unsafe { raw_pointer.read_unaligned() };
            if header.magic != MAGIC {
                return Err(MetadataError::internal("bad metadata header"));
            }
            if header.version != VERSION {
                // Handle version change when modifying VERSION.
                return Err(MetadataError::internal("bad metadata header"));
            }
        }

        Ok(())
    }

    pub fn walk<F>(&self, mut work: F) -> Result<(), MetadataError>
    where
        F: FnMut(&[u8], &[u8], MetadataType),
    {
        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        let mut offset = header_size;
        while offset + item_size < self.buffer.len() {
            let buffer = self.buffer.as_ref();
            let raw_pointer = unsafe { buffer.as_ptr().add(offset).cast::<MetadataItem>() };
            let item: MetadataItem = unsafe { raw_pointer.read_unaligned() };

            let key_length = item.key_length as usize;
            let value_length = item.value_length as usize;
            let value_type = item.value_type;
            offset += item_size;

            if offset + key_length + value_length > buffer.len() {
                break;
            }

            let key_data = unsafe { buffer.as_ptr().add(offset) };
            let key_slice = unsafe { std::slice::from_raw_parts(key_data, key_length) };
            offset += key_length;

            let value_data = unsafe { buffer.as_ptr().add(offset) };
            let value_slice = unsafe { std::slice::from_raw_parts(value_data, value_length) };
            offset += value_length;

            let value_type = MetadataType::from(value_type);
            work(key_slice, value_slice, value_type);
        }

        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

unsafe impl Send for Metadata {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_string_rejects_truncated_utf8() {
        // \xe4\xb8 is a truncated 3-byte UTF-8 sequence (missing final byte)
        let bad_utf8: &[u8] = b"hello \xe4\xb8";
        let result = Metadata::to_string(bad_utf8);
        assert!(result.is_err());
    }

    #[test]
    fn walk_with_invalid_utf8_key() {
        let mut metadata = Metadata::new();
        // Store a value with an invalid UTF-8 key using the private `set` method
        metadata
            .set(b"\xe4\xb8", b"value", MetadataType::String)
            .unwrap();

        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        metadata
            .walk(|key, value, _value_type| {
                entries.push((key.to_vec(), value.to_vec()));
            })
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, b"\xe4\xb8");
        assert_eq!(entries[0].1, b"value");
    }

    #[test]
    fn get_with_invalid_utf8_key() {
        let mut metadata = Metadata::new();
        metadata
            .set(b"\xe4\xb8", b"value", MetadataType::String)
            .unwrap();

        // get() works on raw &[u8] keys, so it should find the value
        let result = metadata.get(b"\xe4\xb8").unwrap();
        assert_eq!(result, b"value");
    }

    #[test]
    fn decode_to_value_numeric() {
        let result = Metadata::decode_to_value("42", &MetadataType::Numeric).unwrap();
        assert_eq!(result, 42u64.to_le_bytes().to_vec());
    }

    #[test]
    fn decode_to_value_numeric_zero() {
        let result = Metadata::decode_to_value("0", &MetadataType::Numeric).unwrap();
        assert_eq!(result, 0u64.to_le_bytes().to_vec());
    }

    #[test]
    fn decode_to_value_numeric_max() {
        let max = u64::MAX.to_string();
        let result = Metadata::decode_to_value(&max, &MetadataType::Numeric).unwrap();
        assert_eq!(result, u64::MAX.to_le_bytes().to_vec());
    }

    #[test]
    fn decode_to_value_numeric_invalid() {
        let result = Metadata::decode_to_value("not_a_number", &MetadataType::Numeric);
        assert!(result.is_err());
    }

    #[test]
    fn decode_to_value_numeric_negative() {
        let result = Metadata::decode_to_value("-1", &MetadataType::Numeric);
        assert!(result.is_err());
    }

    #[test]
    fn decode_to_value_numeric_overflow() {
        let overflow = format!("{}0", u64::MAX);
        let result = Metadata::decode_to_value(&overflow, &MetadataType::Numeric);
        assert!(result.is_err());
    }

    #[test]
    fn decode_to_value_string() {
        let result = Metadata::decode_to_value("hello", &MetadataType::String).unwrap();
        assert_eq!(result, b"hello");
    }

    #[test]
    fn decode_to_value_binary() {
        let result = Metadata::decode_to_value("raw data", &MetadataType::Binary).unwrap();
        assert_eq!(result, b"raw data");
    }
}
