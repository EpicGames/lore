// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod branch;
pub mod clear;
pub mod find;
pub mod get;
pub mod list;
pub mod repository;
pub mod set;

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use bytes::BytesMut;
use futures::FutureExt;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_error_set::prelude::*;
use lore_storage::ContentSource;
use lore_storage::ReadOptions;
use zerocopy::IntoBytes;

use crate::errors::*;
use crate::event::EventError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::with_operation;
use crate::immutable;
use crate::interface::LoreError;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util::path::RelativePath;
use crate::util::path::is_path_inside_repository;
use crate::util::path::make_absolute;

/// Maximum serialized metadata blob size. Metadata is loaded fully into memory
/// at deserialize time; callers needing to attach larger data should store it
/// as a separate immutable blob and reference it via an [`Address`] or [`Hash`]
/// value in the metadata.
///
/// A single entry larger than this is refused when it is set, since no amount of
/// removing other keys could make it fit. The total is checked when the metadata
/// is serialized, which is the only point at which it is known.
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
            MetadataErrors::FileNotFound(_) => LoreError::NotFound,
            MetadataErrors::Oversized(_) => LoreError::Oversized,
            MetadataErrors::Disconnected(_) => LoreError::Connection,
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
/// Origin of the work in the revision ([`MetadataType::String`]). Set by the
/// caller or carried by an inherit list; a commit fills it only when unset.
pub const CREATED_BY: &str = "created-by";
/// Who put the revision into the chain ([`MetadataType::String`]). Always the
/// committer, so never inheritable.
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

/// Keys describing the operation that creates a revision rather than the work
/// it records, written by that operation.
///
/// Never inheritable, so [`MetadataInherit::All`] cannot forward the attribution
/// an inherit list exists to govern.
pub const RESERVED_STAMP: [&str; 5] = [MESSAGE, TIMESTAMP, BRANCH, COMMITTED_BY, MERGED_BY];

/// Keys recording an operation the new revision is not.
///
/// Never inheritable: a merge holding `cherry-picked-from` claims to be a
/// cherry-pick. Each is written by the operation that owns it, after the
/// inherit filter runs.
pub const RESERVED_ERASE: [&str; 4] = [
    CHERRY_PICKED_FROM,
    REVERTED_FROM,
    RESTORED_FROM,
    FAST_FORWARD_MERGE,
];

/// The address the file a user named as a binary metadata value was stored at.
///
/// A path inside the repository is read through an operation on it, so a virtual filesystem
/// answers for it, and the store reads it under that operation. One outside is read from the host
/// filesystem: a value is free to name a file the user never put in a repository at all. Only a
/// relative path needs a working tree to resolve against, so an absolute one is read from a
/// context holding none -- a server, an in-memory handle -- rather than refused.
///
/// Streamed from where it sits rather than read whole, so what a value may name is bounded by the
/// store and not by memory. Written under a zero context, so content that several revisions or
/// files name keeps one entry.
pub(crate) async fn store_binary_payload<E>(
    repository: &Arc<RepositoryContext>,
    value: &[u8],
) -> Result<Address, E>
where
    E: ErrorSet + From<InvalidArguments>,
{
    let user_path = String::from_utf8_lossy(value);

    let store = async |source: ContentSource<'static>| {
        immutable::write_from_file(
            repository.clone(),
            &source,
            Context::default(),
            immutable::write_options_from_repository(repository.clone()),
        )
        .await
        .map(|(address, _size)| address)
        .forward_any::<E>("Failed to store the binary metadata payload")
    };

    let repository_path = if Path::new(user_path.as_ref()).is_absolute() {
        repository.path()
    } else {
        Some(repository.require_path()?)
    };

    if let Some(repository_path) =
        repository_path.filter(|root| is_path_inside_repository(root, &user_path))
    {
        let path = RelativePath::new_from_user_path(repository_path, &user_path)
            .forward_any::<E>("Failed to resolve the binary metadata path")?;
        return with_operation(repository.file_system(), async |operation| {
            let source = operation
                .content_source(&path)
                .await
                .forward_any::<E>("naming the content of the binary metadata payload")?;
            store(source).await
        })
        .await;
    }

    let outside =
        make_absolute(&user_path).forward_any::<E>("Failed to resolve the binary metadata path")?;
    store(ContentSource::owned_file(outside)).await
}

/// The node the user path `path` names in `state`, refused where `path` is inside a link: a node
/// reached through a link is numbered in the linked tree, so `state` holds another file's metadata
/// under it.
pub(crate) async fn file_node_outside_links(
    state: &State,
    repository: &Arc<RepositoryContext>,
    path: &str,
) -> Result<NodeID, MetadataErrors> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path)
        .forward_any::<MetadataErrors>("resolving user path")?;
    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_matched_err("finding node", |m| match m {
            MatchedStateErrors::NodeNotFound(traced) => MetadataErrors::FileNotFound(
                FileNotFound {
                    resource: path.to_string(),
                }
                .chain_err(traced, "the revision does not hold the path"),
            ),
            other => other.forward::<MetadataErrors>("finding node"),
        })?;
    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: path.to_string(),
        }
        .into());
    }
    if node_link.crosses_link(repository, state) {
        return Err(NotSupported {
            operation: format!("file metadata on `{relative_path}`, which is inside a link"),
        }
        .into());
    }
    Ok(node_link.node)
}

/// Which of a source revision's keys a merge or cherry-pick carries onto the
/// revision it creates, so the result reads as the integrated sum of the work
/// brought in.
///
/// Supplied per operation, since whether a key is a durable property of the work
/// or an assertion about one revision is not something its name reveals. The
/// default carries nothing.
#[derive(Clone, Debug)]
pub enum MetadataInherit {
    /// Carry only the named keys.
    Keys(Vec<String>),
    /// Carry every key the reserved sets above allow.
    All,
}

impl Default for MetadataInherit {
    fn default() -> Self {
        MetadataInherit::Keys(Vec::new())
    }
}

impl MetadataInherit {
    /// The sentinel key name selecting [`MetadataInherit::All`].
    pub const ALL: &'static str = "*";

    /// Build from caller-supplied key names. [`Self::ALL`] anywhere in `keys`
    /// selects [`MetadataInherit::All`].
    pub fn from_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut names = Vec::new();
        for key in keys {
            let key = key.as_ref();
            if key == Self::ALL {
                return MetadataInherit::All;
            }
            names.push(key.to_string());
        }
        MetadataInherit::Keys(names)
    }

    /// Whether `key` survives onto the new revision.
    pub fn permits(&self, key: &str) -> bool {
        if RESERVED_STAMP.contains(&key) || RESERVED_ERASE.contains(&key) {
            return false;
        }
        match self {
            MetadataInherit::All => true,
            MetadataInherit::Keys(keys) => keys.iter().any(|named| named == key),
        }
    }

    /// Whether this carries nothing, the default.
    pub fn is_empty(&self) -> bool {
        matches!(self, MetadataInherit::Keys(keys) if keys.is_empty())
    }
}

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

/// A set of keyed values, held as the buffer it is stored as. What it describes
/// is up to whoever attached it — a revision, a branch, a repository, a file.
///
/// The buffer's entry chain always stays inside it: one built here is written
/// that way, and one read from the store is checked by [`Self::check_buffer`]
/// before anything reads it. The accessors walk the chain with raw pointers and
/// trust the stored lengths on the strength of that.
#[lore_macro::test_pub]
#[derive(Debug)]
pub struct Metadata {
    buffer: BytesMut,
}

/// The kind of a stored value, under the storage layer's name for it. The same
/// type the API surface carries, not a parallel one, so the tag a caller passes
/// and the tag written into the buffer cannot disagree.
pub use crate::interface::LoreMetadataType as MetadataType;

impl TryFrom<u32> for MetadataType {
    type Error = MetadataError;

    /// Read a tag back out of a metadata buffer.
    ///
    /// A tag this does not recognize is refused rather than coerced to a
    /// default: the buffer was written by something that disagrees with this
    /// build about what the tag means, and guessing would hand the caller a
    /// value of the wrong type instead of telling it the metadata is unreadable.
    fn try_from(tag: u32) -> Result<Self, Self::Error> {
        const ADDRESS: u32 = MetadataType::Address as u32;
        const BOOLEAN: u32 = MetadataType::Boolean as u32;
        const CONTEXT: u32 = MetadataType::Context as u32;
        const HASH: u32 = MetadataType::Hash as u32;
        const NUMERIC: u32 = MetadataType::Numeric as u32;
        const STRING: u32 = MetadataType::String as u32;
        const BINARY: u32 = MetadataType::Binary as u32;

        match tag {
            ADDRESS => Ok(MetadataType::Address),
            BOOLEAN => Ok(MetadataType::Boolean),
            CONTEXT => Ok(MetadataType::Context),
            HASH => Ok(MetadataType::Hash),
            NUMERIC => Ok(MetadataType::Numeric),
            STRING => Ok(MetadataType::String),
            BINARY => Ok(MetadataType::Binary),
            _ => Err(MetadataError::internal("unknown metadata value type")),
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
        Metadata::new_with_buffer(self.buffer.clone())
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
    /// Size of [`MetadataHeader`] at the start of the buffer.
    #[lore_macro::test_pub]
    const HEADER_SIZE: usize = std::mem::size_of::<u32>() * 2;
    /// Size of the [`MetadataItem`] header preceding each entry's bytes.
    #[lore_macro::test_pub]
    const ITEM_SIZE: usize = std::mem::size_of::<u32>() * 3;

    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(DEFAULT_METADATA_CAPACITY),
        }
    }

    fn new_with_buffer(buffer: BytesMut) -> Self {
        Self { buffer }
    }

    /// Read a stored metadata blob as [`Self::deserialize_with`] does, with the read options of
    /// `repository` and the cache on.
    ///
    /// Returns the future of [`Self::deserialize_with`], without a future of its own.
    #[inline]
    pub fn deserialize(
        repository: Arc<RepositoryContext>,
        hash: Hash,
    ) -> impl Future<Output = Result<Self, MetadataError>> {
        let options = immutable::read_options_from_repository(&repository).with_cache();
        Self::deserialize_with(repository, hash, options)
    }

    /// Read a stored metadata blob with `options`, refusing one larger than
    /// [`METADATA_MAX_SIZE`].
    ///
    /// This is the only way bytes this process did not write become a
    /// [`Metadata`], so it is the one place the entry chain has to be checked —
    /// a buffer built by [`Self::set`] is correct by construction and pays
    /// nothing. The check is one pass of integer arithmetic over the entries,
    /// against a fetch that dominates it.
    ///
    /// Returns the read's future, built at the call, with the check mapped onto its output, so it
    /// holds nothing beside the read.
    #[inline]
    pub fn deserialize_with(
        repository: Arc<RepositoryContext>,
        hash: Hash,
        options: ReadOptions,
    ) -> impl Future<Output = Result<Self, MetadataError>> {
        let address = Address {
            hash,
            context: Context::default(),
        };
        immutable::read(
            repository,
            address,
            None, /* No range, read all */
            options.with_max_content_size(METADATA_MAX_SIZE as u64),
        )
        .map(|read| -> Result<Self, MetadataError> {
            let buffer = read.forward::<MetadataError>("reading metadata")?;
            let metadata = Metadata::new_with_buffer(BytesMut::from(buffer));
            metadata.check_buffer()?;
            Ok(metadata)
        })
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
        let address = immutable::write_with_tracker(
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
        let address = immutable::write(
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
            // Spelled as an explicit `TryFrom` so the error type is resolved
            // here. Left to inference it stays an inference variable, and
            // `.internal()` then matches both `WrapInternal` and the
            // error-set guard, which is an ambiguity rather than a real
            // finding: the error is `TryFromSliceError`, a foreign type.
            Ok(u64::from_le_bytes(
                <[u8; 8]>::try_from(value).internal("metadata type mismatch")?,
            ))
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    /// Decode caller-supplied text into the stored byte form for `format`.
    ///
    /// For the verbs whose API takes text plus a separate format tag —
    /// `lore_revision_metadata_set` and its file, branch and repository
    /// siblings. The revision-tree verbs take a typed value instead and never
    /// reach this.
    ///
    /// Every type with a byte encoding of its own is parsed here, so a value
    /// stored under a tag really holds that type: reading it back through
    /// [`Self::get_typed`] and the matching `to_*` helper round-trips. Text that
    /// does not parse is refused rather than stored raw. `String` and `Binary`
    /// keep the text's own bytes, which is what those types mean — so a binary
    /// value set this way can only carry bytes that are valid text, which is one
    /// reason the revision-tree verbs take a typed value.
    pub fn decode_to_value(value: &str, format: &MetadataType) -> Result<Vec<u8>, MetadataError> {
        match format {
            MetadataType::Numeric => {
                let parsed: u64 = value
                    .parse()
                    .map_err(|_parse_err| MetadataError::internal("invalid numeric value"))?;
                Ok(parsed.to_le_bytes().to_vec())
            }
            MetadataType::Boolean => {
                let parsed = match value.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => true,
                    "false" | "0" | "no" | "off" => false,
                    _ => return Err(MetadataError::internal("invalid boolean value")),
                };
                Ok(vec![u8::from(parsed)])
            }
            MetadataType::Address => {
                let parsed = Address::from_str(value)
                    .map_err(|_parse_err| MetadataError::internal("invalid address value"))?;
                Ok(parsed.as_bytes().to_vec())
            }
            MetadataType::Hash => {
                let parsed = Hash::from_str(value)
                    .map_err(|_parse_err| MetadataError::internal("invalid hash value"))?;
                Ok(parsed.data().to_vec())
            }
            MetadataType::Context => {
                let parsed = Context::from_str(value)
                    .map_err(|_parse_err| MetadataError::internal("invalid context value"))?;
                Ok(parsed.data().to_vec())
            }
            MetadataType::String | MetadataType::Binary => Ok(value.as_bytes().to_vec()),
        }
    }

    pub fn to_bool(value: &[u8]) -> Result<bool, MetadataError> {
        if value.len() == 1 {
            Ok(value[0] != 0)
        } else {
            Err(MetadataError::internal("metadata type mismatch"))
        }
    }

    #[lore_macro::test_pub]
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
                return Ok((value_slice, MetadataType::try_from(value_type)?));
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

    /// Stores already-encoded value bytes under `key` with an explicit
    /// [`MetadataType`], the write-side mirror of [`Self::get_typed`].
    ///
    /// The typed setters above cover values Rust code holds as Rust types. A
    /// caller that arrives with encoded bytes and a separate type tag — anything
    /// crossing an FFI boundary — has no Rust type to dispatch on and needs this.
    pub fn set_typed(
        &mut self,
        key: &str,
        value: &[u8],
        value_type: MetadataType,
    ) -> Result<(), MetadataError> {
        self.set(key.as_bytes(), value, value_type)
    }

    /// Remove a key from the metadata. Returns `true` if the key existed.
    pub fn remove_key(&mut self, key: &str) -> bool {
        self.remove(key.as_bytes())
    }

    /// Keep the entries `inherit` permits, drop the rest, and answer how many
    /// were dropped. Dropping none leaves the buffer byte-identical.
    ///
    /// Carrying nothing empties the buffer, which serializes to the zero hash a
    /// revision without metadata holds. A key that is not valid UTF-8 cannot be
    /// named in an inherit list, so it is never permitted.
    pub fn retain_inherited(&mut self, inherit: &MetadataInherit) -> usize {
        let dropped = self
            .retain_entries(|key| std::str::from_utf8(key).is_ok_and(|key| inherit.permits(key)));
        if self.buffer.len() == Self::HEADER_SIZE {
            self.buffer.clear();
        }
        dropped
    }

    /// Move the entries in `[from, to)` down to `write`, answering the cursor
    /// past them.
    fn shift_down(&mut self, from: usize, to: usize, write: usize) -> usize {
        if from == to {
            return write;
        }
        if write != from {
            self.buffer.copy_within(from..to, write);
        }
        write + (to - from)
    }

    /// Compact the entry chain down to the entries `keep` accepts, answering how
    /// many were dropped.
    ///
    /// Consecutive retained entries move as one block, so a single removal costs
    /// one copy and keeping or dropping everything costs none. An entry whose
    /// lengths reach past the buffer ends the walk, as in [`Self::walk`].
    #[lore_macro::test_pub]
    fn retain_entries(&mut self, mut keep: impl FnMut(&[u8]) -> bool) -> usize {
        if self.is_empty() {
            return 0;
        }

        let mut dropped = 0;
        let mut read = Self::HEADER_SIZE;
        let mut write = Self::HEADER_SIZE;
        let mut retained = Self::HEADER_SIZE;
        while read + Self::ITEM_SIZE <= self.buffer.len() {
            // SAFETY: the entry header fits, as just checked.
            let item: MetadataItem = unsafe {
                self.buffer
                    .as_ptr()
                    .add(read)
                    .cast::<MetadataItem>()
                    .read_unaligned()
            };
            let key_length = item.key_length as usize;
            let block = Self::ITEM_SIZE + key_length + item.value_length as usize;
            if read + block > self.buffer.len() {
                break;
            }

            let key_at = read + Self::ITEM_SIZE;
            if keep(&self.buffer[key_at..key_at + key_length]) {
                read += block;
            } else {
                write = self.shift_down(retained, read, write);
                read += block;
                retained = read;
                dropped += 1;
            }
        }

        write = self.shift_down(retained, read, write);
        self.buffer.truncate(write);
        dropped
    }

    fn remove(&mut self, key: &[u8]) -> bool {
        self.retain_entries(|stored| stored != key) > 0
    }

    /// How large a buffer holding this pair and nothing else would be: the
    /// buffer's own header, the pair's entry header, and the bytes.
    #[lore_macro::test_pub]
    fn stored_size(key_length: usize, value_length: usize) -> usize {
        Self::HEADER_SIZE + Self::ITEM_SIZE + key_length + value_length
    }

    /// Whether metadata could ever hold this pair, given [`METADATA_MAX_SIZE`].
    ///
    /// Answers about the pair alone, not about what a buffer already carries: a
    /// pair this refuses cannot be stored however much else is removed, while
    /// one it accepts may still push a particular buffer past the limit. That
    /// total is only known when the metadata is serialized.
    pub fn can_hold(key: &str, value: &[u8]) -> bool {
        Self::stored_size(key.len(), value.len()) <= METADATA_MAX_SIZE
    }

    #[lore_macro::test_pub]
    fn set(
        &mut self,
        key: &[u8],
        value: &[u8],
        value_type: MetadataType,
    ) -> Result<(), MetadataError> {
        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        let entry_size = Self::stored_size(key.len(), value.len());
        if entry_size > METADATA_MAX_SIZE {
            return Err(MetadataError::from(Oversized {
                context: format!(
                    "metadata entry needing {entry_size} bytes exceeds the whole \
                     {METADATA_MAX_SIZE} byte metadata limit; store large values as \
                     separate blobs and reference them via hash"
                ),
            }));
        }

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
                        let tag = value_type as u32;
                        std::ptr::copy_nonoverlapping(
                            std::ptr::addr_of!(tag).cast::<u8>(),
                            buffer
                                .as_mut_ptr()
                                .add(start_offset + std::mem::size_of::<u32>() * 2),
                            std::mem::size_of::<u32>(),
                        );
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

    /// Establish that the buffer is a metadata blob this build can read, and
    /// that its entry chain stays inside it.
    ///
    /// Every accessor walks the chain with raw pointers and takes the stored
    /// lengths at their word, which is only sound because a buffer that reached
    /// one has been through here. Bytes arriving from the store are checked once
    /// on the way in rather than on every lookup, so a blob that was truncated
    /// or written by something that disagrees with this layout is refused
    /// instead of read past.
    #[lore_macro::test_pub]
    fn check_buffer(&self) -> Result<(), MetadataError> {
        let buffer_size = self.buffer.len();
        if buffer_size == 0 {
            return Ok(());
        }

        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;
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

        // Each bound is checked against the room left before it is consumed, so
        // `offset` only ever lands on or before the end and the subtractions
        // cannot wrap.
        let mut offset = header_size;
        while offset < buffer_size {
            if item_size > buffer_size - offset {
                return Err(MetadataError::internal("truncated metadata entry"));
            }
            // SAFETY: the entry header fits, as just checked.
            let item: MetadataItem = unsafe {
                buffer
                    .as_ptr()
                    .add(offset)
                    .cast::<MetadataItem>()
                    .read_unaligned()
            };
            offset += item_size;

            let payload = item.key_length as usize + item.value_length as usize;
            if payload > buffer_size - offset {
                return Err(MetadataError::internal(
                    "metadata entry overruns the buffer",
                ));
            }
            offset += payload;
        }

        Ok(())
    }

    /// Visit every entry this build can read, in stored order.
    ///
    /// An entry stored under a kind this build does not know is passed over
    /// rather than ending the walk: the keys either side of it are still
    /// readable, and a kind that cannot be named here is not one a caller could
    /// have acted on. A caller that must know a specific key was unreadable
    /// asks for it by name through [`Self::get_typed`], which refuses it.
    ///
    /// Cannot fail: a buffer read from the store was checked on the way in and
    /// one built here is written entry by entry, so there is nothing left for
    /// the walk itself to reject.
    pub fn walk<F>(&self, mut work: F)
    where
        F: FnMut(&[u8], &[u8], MetadataType),
    {
        let header_size = std::mem::size_of::<u32>() * 2;
        let item_size = std::mem::size_of::<u32>() * 3;

        let mut offset = header_size;
        while offset + item_size <= self.buffer.len() {
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

            let Ok(value_type) = MetadataType::try_from(value_type) else {
                continue;
            };
            work(key_slice, value_slice, value_type);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

unsafe impl Send for Metadata {}
