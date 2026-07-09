// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod diff;
pub mod dump;
mod sink;

use core::str;
use std::future::Future;
use std::io::Write;
use std::mem::size_of;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use bytes::Bytes;
use lore_base::error::InvalidPath;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
pub use sink::ChangeSink;
pub use sink::OwnedChangeSink;
use tokio::join;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use zerocopy::FromZeros;
use zerocopy::Immutable;

use crate::bitflagsops;
use crate::branch;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::errors::LinkNotFound;
use crate::errors::NodeNotFound;
use crate::errors::NotFound;
use crate::errors::Oversized;
use crate::errors::StateErrors;
use crate::filter::FilterMode;
use crate::fragment::FragmentFlags;
use crate::hash;
use crate::immutable;
use crate::immutable::ImmutableError;
use crate::immutable::ReadBoxFromImmutable;
use crate::immutable::ReadFromImmutable;
use crate::immutable::WriteToImmutable;
use crate::immutable::read_options_from_repository;
use crate::instance::InstanceId;
use crate::interface::LoreString;
use crate::link::LinkFlags;
use crate::lore::*;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::nametable::NameTable;
use crate::node;
use crate::node::*;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision::RevisionMetadata;
use crate::stage::stage_delete;
use crate::state::diff::NodeSearchResult;
use crate::state::diff::get_filtered_node_and_path;
use crate::state::diff::get_node_and_path;
use crate::store::KeyType;
use crate::store::StoreMatch;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// Data for an event summarizing a dumped repository state.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStateDumpEventData {
    /// Sequence number of the revision.
    pub revision_number: u64,
    /// Hash of the revision.
    pub revision: Hash,
    /// Hash of the state's node tree.
    pub tree_hash: Hash,
    /// Size of the node tree in bytes.
    pub tree_size: u64,
}

/// Data for an event describing a single node in a dumped repository state.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStateDumpNodeEventData {
    /// Name of the node.
    pub name: LoreString,
    /// Identifier of the node.
    pub id: u32,
    /// Identifier of the parent node.
    pub parent: u32,
    /// Identifier of the next sibling node.
    pub sibling: u32,
    /// File mode of the node.
    pub mode: u16,
    /// Size of the node's content in bytes.
    pub size: u64,
    /// Node flags.
    pub flags: u16,
    /// Type-specific detail for the node.
    pub type_data: LoreString,
}

pub type StateError = StateErrors;

#[derive(Debug)]
pub struct StateNamedNode {
    node: NodeID,
    name: u64,
}

pub struct StateChildrenNodes {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub children: Vec<StateNamedNode>,
}

#[derive(Debug)]
pub struct StateNamedStringNode {
    pub node: NodeID,
    pub name: u64,
    pub name_string: String,
}

pub struct StateNamedChildrenNodes {
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub children: Vec<StateNamedStringNode>,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct StateFlags: u32 {
        /// No flags
        const NoFlags = 0;
        /// State is dirty
        const Dirty = 0b1;
        /// State is in conflict
        const Conflict = 0b10;
        /// State is merged (branch merge)
        const Merge = 0b100;
        /// State is cherry-picked
        const CherryPick = 0b1000;
        /// State is a revert operation
        const Revert = 0b10000;
    }
}
bitflagsops!(StateFlags, u32);

/// Iterator over child nodes of a directory, loading blocks with nametable.
/// Yields `(NodeID, Node, NodeNameLock)` — the child's ID, node data, and name.
///
/// The yielded [`NodeNameLock`] holds a read lock on the node block (zero-copy
/// name access), not an owned string. Drop it before any call that may take
/// another lock — recursing into this iterator, or a `State` method that loads a
/// block such as `block_with_nametable`, which can *write*-lock the same block to
/// deserialize its nametable. The locks are not reentrant, so holding the name
/// across such a call (especially an `.await`) risks deadlock. Copy it out with
/// [`NodeNameLock::freeze`] first if you need it past that point.
pub struct StateNodeChildrenWithNameIterator {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node_id: NodeID,
    current_node_id: Option<NodeID>,
    current_block: Option<Arc<NodeBlock>>,
    current_iblock: usize,
    cycle: SiblingCycleGuard,
}

impl StateNodeChildrenWithNameIterator {
    /// Create a new iterator starting from the first child of the given parent node.
    /// Loads blocks with nametable for name lookup via `next()`.
    pub async fn new(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        parent_node_id: NodeID,
    ) -> Result<Self, StateError> {
        if !parent_node_id.is_valid_or_root_node_id() {
            return Ok(Self {
                state,
                repository,
                parent_node_id,
                current_node_id: None,
                current_block: None,
                current_iblock: 0,
                cycle: SiblingCycleGuard::new(parent_node_id),
            });
        }
        let parent = state.node(repository.clone(), parent_node_id).await?;
        let first_child = parent.child();

        let (block, iblock) = if let Some(child_id) = first_child {
            let iblock = NodeBlock::index(child_id);
            let block = state
                .block_with_nametable(repository.clone(), iblock)
                .await?;
            (Some(block), iblock)
        } else {
            (None, 0)
        };

        Ok(Self {
            state,
            repository,
            parent_node_id,
            current_node_id: first_child,
            current_block: block,
            current_iblock: iblock,
            cycle: SiblingCycleGuard::new(parent_node_id),
        })
    }

    /// Get the next child node with its name.
    ///
    /// The returned [`NodeNameLock`] holds a read lock on the node block. It is
    /// `Send`, but drop it before any call that may take another lock (see the
    /// type docs) — copy it out with [`NodeNameLock::freeze`] if needed.
    pub async fn next(&mut self) -> Result<Option<(NodeID, Node, NodeNameLock)>, StateError> {
        loop {
            let Some(node_id) = self.current_node_id else {
                return Ok(None);
            };

            let iblock = NodeBlock::index(node_id);
            if iblock != self.current_iblock || self.current_block.is_none() {
                self.current_iblock = iblock;
                self.current_block = Some(
                    self.state
                        .block_with_nametable(self.repository.clone(), iblock)
                        .await?,
                );
            }

            let block = self.current_block.as_ref().unwrap();
            let node_index = Node::index(node_id);
            let node = block.node(node_index);
            node.walk_step(node_id, self.parent_node_id, &mut self.cycle)?;
            self.current_node_id = node.sibling();
            match block.node_name_ref(node_index) {
                Ok(name) => return Ok(Some((node_id, node, name))),
                Err(err) => {
                    lore_warn!("Skipping node {node_id} with invalid name: {err}");
                }
            }
        }
    }
}

/// Iterator over child nodes of a directory, loading blocks without nametable.
/// Yields `(NodeID, Node)` — the child's ID and node data, without the name string.
pub struct StateNodeChildrenIterator {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node_id: NodeID,
    current_node_id: Option<NodeID>,
    current_block: Option<Arc<NodeBlock>>,
    current_iblock: usize,
    cycle: SiblingCycleGuard,
}

impl StateNodeChildrenIterator {
    /// Create a new iterator starting from the first child of the given parent node.
    /// Loads blocks without nametable — use when only node data is needed.
    pub async fn new(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        parent_node_id: NodeID,
    ) -> Result<Self, StateError> {
        if !parent_node_id.is_valid_or_root_node_id() {
            return Ok(Self {
                state,
                repository,
                parent_node_id,
                current_node_id: None,
                current_block: None,
                current_iblock: 0,
                cycle: SiblingCycleGuard::new(parent_node_id),
            });
        }
        let parent = state.node(repository.clone(), parent_node_id).await?;
        let first_child = parent.child();

        let (block, iblock) = if let Some(child_id) = first_child {
            let iblock = NodeBlock::index(child_id);
            let block = state.block(repository.clone(), iblock).await?;
            (Some(block), iblock)
        } else {
            (None, 0)
        };

        Ok(Self {
            state,
            repository,
            parent_node_id,
            current_node_id: first_child,
            current_block: block,
            current_iblock: iblock,
            cycle: SiblingCycleGuard::new(parent_node_id),
        })
    }

    /// Get the next child node.
    pub async fn next(&mut self) -> Result<Option<(NodeID, Node)>, StateError> {
        let Some(node_id) = self.current_node_id else {
            return Ok(None);
        };

        let iblock = NodeBlock::index(node_id);
        if iblock != self.current_iblock || self.current_block.is_none() {
            self.current_iblock = iblock;
            self.current_block = Some(self.state.block(self.repository.clone(), iblock).await?);
        }

        let block = self.current_block.as_ref().unwrap();
        let node_index = Node::index(node_id);
        let node = block.node(node_index);
        node.walk_step(node_id, self.parent_node_id, &mut self.cycle)?;

        self.current_node_id = node.sibling();

        Ok(Some((node_id, node)))
    }
}

/// Revision state control structure, internally mutable through r/w locks
pub struct State {
    /// Serialized data
    data: parking_lot::RwLock<StateData>,
    /// Runtime in memory data
    runtime: parking_lot::RwLock<StateRuntime>,
    /// Deserializing semaphore
    deserialize: tokio::sync::Semaphore,
    /// Unused node/block semaphore
    unused: tokio::sync::Semaphore,
    /// Block deserialization semaphore
    block_deserialize: tokio::sync::Semaphore,
    /// File metadata block deserialization semaphore
    metadata_deserialize: tokio::sync::Semaphore,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "State({})", self.runtime.read().signature)
    }
}

/// Mutable store function for file timestamp
const FILE_MTIME: &str = "file-mtime";

/// Magic identifier
const STATE_MAGIC: u32 = 0xD37A208Eu32;

/// State format version identifiers
#[repr(u32)]
pub enum StateFormat {
    /// Initial version
    Initial = 1,
    /// Node name hash is lower case
    LowerCaseHash = 2,
}

#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct StateData {
    /// Magic identifier
    magic: u32,
    /// Format version
    format: u32,
    /// State flags
    flags: u32,
    /// Reserved for future extensions
    reserved_header: u32,
    /// Reserved for future extensions
    reserved_uint32: [u32; 2],
    /// Revision number
    pub revision_number: u64,
    /// Parent state signatures
    pub parent: [Hash; 2],
    /// Immutable merkle tree fragment
    hash_tree: Hash,
    /// Immutable metadata fragment
    hash_metadata: Hash,
    /// Immutable link list
    hash_link: Hash,
    /// Link merge state (transient, local only — zeroed before commit)
    hash_link_merge: Hash,
    /// Reserved for future extensions
    hash_reserved: Hash,
    /// Parent repository in case of merge/integrate from other repository
    parent_repository: RepositoryId,
    /// Unused (for future extension)
    reserved_buffer_first: [u8; 16],
    /// Unused (for future extension)
    reserved_buffer_second: [u8; 32],
}

#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct LinkReference {
    /// Repository identifier
    pub(crate) repository: RepositoryId,
    /// Branch identifier
    pub(crate) branch: BranchId,
    /// Revision signature
    pub(crate) signature: Hash,
    /// Node containing the link
    pub(crate) local_node: u32,
    /// Flags
    pub(crate) flags: u32,
    /// Unused
    pub(crate) unused: u32,
}

impl LinkReference {
    pub fn resolve_branch(&self, parent_branch: BranchId) -> BranchId {
        if self.branch.is_zero() {
            parent_branch
        } else {
            self.branch
        }
    }
}

/// Tracks a single link's merge state for rollback.
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct LinkMergeEntry {
    /// Link path node ID (correlates with `LinkReference.local_node`)
    pub local_node: u32,
    /// Reserved for future use
    pub reserved: u32,
    /// Pre-merge (base) link reference snapshot for rollback
    pub base: LinkReference,
}

/// Header for the serialized link merge state blob.
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct LinkMergeState {
    /// Number of `LinkMergeEntry` items following this header
    pub count: u32,
    /// Flags (reserved for future use)
    pub flags: u32,
}
const MAX_BLOCK_CACHE: usize = 5000;

struct StateRuntime {
    /// Signature state was deserialized from
    signature: Hash,
    /// Deserialized merkle tree data
    tree: Option<Tree>,
    /// Memory buffer holding all block addresses
    block_address: Bytes,
    /// Weak references to each block
    block: Vec<Weak<NodeBlock>>,
    /// Dirty blocks kept in memory
    block_dirty: Vec<(Arc<NodeBlock>, usize)>,
    /// Cached blocks kept in memory
    block_cache: Vec<Arc<NodeBlock>>,
    /// Cache counter
    block_cache_counter: AtomicU64,
    /// Memory buffer holding all file metadata block addresses
    block_file_metadata_address: Bytes,
    /// Weak references to each file metadata block
    block_file_metadata: Vec<Weak<NodeFileMetadataBlock>>,
    /// Dirty blocks kept in memory
    block_file_metadata_dirty: Vec<(Arc<NodeFileMetadataBlock>, usize)>,
    /// Link list
    link_list: Option<Vec<LinkReference>>,
    /// Name table (read only, for old data formats)
    name_table_deprecated: Option<Arc<NameTable>>,
    /// Rehash node names
    rehash_node_names: bool,
}

impl StateRuntime {
    pub fn new(signature: Hash, rehash_node_names: bool) -> Self {
        StateRuntime {
            signature,
            tree: None,
            block_address: Bytes::default(),
            block: vec![],
            block_dirty: vec![],
            block_cache: vec![],
            block_cache_counter: AtomicU64::new(0),
            block_file_metadata_address: Bytes::default(),
            block_file_metadata: vec![],
            block_file_metadata_dirty: vec![],
            link_list: None,
            name_table_deprecated: None,
            rehash_node_names,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            data: parking_lot::RwLock::new(StateData::new_zeroed()),
            runtime: parking_lot::RwLock::new(StateRuntime::new(Hash::default(), false)),
            unused: tokio::sync::Semaphore::new(1),
            deserialize: tokio::sync::Semaphore::new(1),
            block_deserialize: tokio::sync::Semaphore::new(1),
            metadata_deserialize: tokio::sync::Semaphore::new(1),
        }
    }

    /// Load the current state and branch.
    pub async fn deserialize_current(
        repository: Arc<RepositoryContext>,
    ) -> Result<(Arc<Self>, BranchId), StateError> {
        let (current_revision, branch) = crate::instance::load_current_anchor(&repository)
            .await
            .internal("Failed to deserialize anchor")?;
        Ok((
            State::deserialize(repository.clone(), current_revision).await?,
            branch,
        ))
    }

    /// Load current and optionally staged states, plus the current branch.
    ///
    /// Returns `(current_state, staged_state, branch)` where `staged_state`
    /// is `None` when nothing is staged.
    pub async fn deserialize_current_and_staged(
        repository: Arc<RepositoryContext>,
    ) -> Result<(Arc<Self>, Option<Arc<Self>>, BranchId), StateError> {
        let (current_revision, branch) = crate::instance::load_current_anchor(&repository)
            .await
            .internal("Failed to deserialize anchor")?;
        let state_current = State::deserialize(repository.clone(), current_revision).await?;

        let state_staged = match crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
        {
            Some(staged_revision) if staged_revision != current_revision => {
                Some(State::deserialize(repository.clone(), staged_revision).await?)
            }
            _ => None,
        };

        Ok((state_current, state_staged, branch))
    }

    pub async fn deserialize(
        repository: Arc<RepositoryContext>,
        signature: Hash,
    ) -> Result<Arc<Self>, StateError> {
        if signature.is_zero() {
            return Ok(Arc::new(State::new()));
        }
        let address = Address::zero_context_hash(signature);
        let options = read_options_from_repository(&repository);
        let mut data = match StateData::read_from_immutable(repository, address, options).await {
            Ok(data) => data,
            Err(ref e) if e.is_address_not_found() || e.is_payload_not_found() => {
                return Err(NotFound.into());
            }
            Err(ImmutableError::SlowDown(traced)) => return Err(StateError::SlowDown(traced)),
            Err(_err) => return Err(StateError::internal("Failed to read state data")),
        };

        if data.magic != STATE_MAGIC {
            Err(StateError::internal("Corrupt header"))
        } else if data.format == 0 || data.format > StateFormat::LowerCaseHash as u32 {
            if data.format > StateFormat::LowerCaseHash as u32 && data.format < 0xFFF {
                Err(StateError::internal(format!(
                    "Upgrade format: {}",
                    data.format
                )))
            } else {
                Err(StateError::internal(format!(
                    "Invalid format: {}",
                    data.format
                )))
            }
        } else {
            // If old version, set rehash flag
            let rehash_node_names = data.format < StateFormat::LowerCaseHash as u32;
            // Clean flags
            data.flags &= !StateFlags::Dirty;
            Ok(Arc::new(State {
                data: parking_lot::RwLock::new(data),
                runtime: parking_lot::RwLock::new(StateRuntime::new(signature, rehash_node_names)),
                unused: tokio::sync::Semaphore::new(1),
                deserialize: tokio::sync::Semaphore::new(1),
                block_deserialize: tokio::sync::Semaphore::new(1),
                metadata_deserialize: tokio::sync::Semaphore::new(1),
            }))
        }
    }

    pub async fn serialize(
        &self,
        repository: Arc<RepositoryContext>,
        _token: &RepositoryWriteToken,
    ) -> Result<Hash, StateError> {
        let is_dirty = self.is_dirty();
        if !is_dirty {
            lore_trace!(
                "State not dirtied, return previously serialized signature {}",
                self.revision()
            );
            return Ok(self.revision());
        }

        if self.runtime.read().rehash_node_names {
            // Deserialize all blocks to force update the node name hashes, as state format
            // requires all blocks to have same format
            lore_info!("Updating all state block name hashes");
            let mut tasks = JoinSet::new();
            let mut result = Ok(());
            let static_self = unsafe { extend_lifetime(self) };
            let block_count = self.block_count();
            for block_index in 0..block_count {
                let repository = repository.clone();
                lore_spawn!(tasks, async move {
                    lore_trace!("  block {}/{}", block_index + 1, block_count);
                    let block = static_self.block(repository, block_index).await?;
                    {
                        block.write().mark_dirty();
                    }
                    static_self.block_modified(block, block_index);
                    Ok(())
                });
                if let Some(task_result) = tasks.try_join_next() {
                    match task_result {
                        Ok(inner_result) => {
                            if result.is_ok() {
                                result = inner_result;
                            }
                        }
                        Err(err) => {
                            result = Err(StateError::internal_with_context(err, "Task failure"));
                        }
                    }
                }
            }
            while let Some(task_result) = tasks.join_next().await {
                match task_result {
                    Ok(inner_result) => {
                        if result.is_ok() {
                            result = inner_result;
                        }
                    }
                    Err(err) => {
                        result = Err(StateError::internal_with_context(err, "Task failure"));
                    }
                }
            }
            result?;
        }

        let (block_dirty, block_file_metadata_dirty) = {
            let lock = self.runtime.read();
            (
                lock.block_dirty.clone(),
                lock.block_file_metadata_dirty.clone(),
            )
        };

        let mut tree = self.tree(repository.clone()).await?;
        let block_count = tree.block_count as usize;

        if !block_dirty.is_empty() {
            lore_debug!("Serializing {} dirty blocks", block_dirty.len());
            let mut tasks: JoinSet<Result<(Address, usize), StateError>> = JoinSet::new();
            for (block, block_index) in block_dirty.iter() {
                let block = block.clone();
                let block_index = *block_index;
                if block.read().raw().flags & NodeBlockFlags::FirstUnusedNode != 0 {
                    let block_unused_next = tree.block_unused_first;
                    tree.block_unused_first = block_index as u32;
                    block.write().node_block().block_unused_next = block_unused_next;
                }
                lore_trace!("Queue serialization of dirty node block {}", block_index);
                let repository = repository.clone();
                lore_spawn!(tasks, async move {
                    // TODO(mjansson): Figure out a way to write the node block without having to copy
                    // it out of the lock first. Writing from the locked ref will not work as the immutable
                    // write makes the lock held over an await point
                    lore_trace!("Serializing dirty node block {}", block_index);
                    let mut node_block = {
                        block.deserialize_nametable(repository.clone()).await?;
                        block.node_name_repack();
                        if block.is_nametable_deserialized() {
                            lore_trace!("Serializing dirty node block {} name table", block_index);
                            let name_table = block.read().clone_name_table();
                            let (name_table, _) = if !name_table.is_empty() {
                                immutable::write(
                                    repository.clone(),
                                    Context::default(),
                                    name_table,
                                    immutable::write_options_from_repository(repository.clone())
                                        .with_local_cache_priority()
                                        .with_max_size_chunk(),
                                )
                                .await
                                .internal("Failed to serialize node block")?
                            } else {
                                (Address::default(), Fragment::default())
                            };
                            {
                                let mut writer = block.write();
                                writer.node_block().name_table = name_table.hash;
                            }
                        }
                        block.read().node_block().clone_on_heap()
                    };
                    node_block.flags &= !NodeBlockFlags::Dirty;
                    node_block.flags &= !NodeBlockFlags::UpgradeGeneratedNametable;
                    node_block.flags &= !NodeBlockFlags::FirstUnusedNode;
                    let (address, _) = node_block
                        .write_to_immutable(
                            repository.clone(),
                            Context::default(),
                            immutable::write_options_from_repository(repository.clone())
                                .with_local_cache_priority()
                                .with_max_size_chunk(),
                        )
                        .await
                        .internal("Failed to serialize node block")?;
                    Ok((address, block_index))
                });
            }

            let mut block_hash_bytes = {
                let lock = self.runtime.read();
                // Resize buffer with empty hashes if needed
                lock.block_address
                    .clone_and_resize_zeroed::<Hash>(block_count)
            };
            {
                let block_hash = block_hash_bytes.as_type_slice_mut();

                let mut final_error = Ok(());
                let mut task_error = Ok(());
                while let Some(task) = tasks.join_next().await {
                    if let Ok(result) = task {
                        if let Ok((address, block_index)) = result {
                            block_hash[block_index] = address.hash;
                        } else {
                            final_error = Err(result.unwrap_err());
                        }
                    } else {
                        task_error = Err(StateError::internal_with_context(
                            task.unwrap_err(),
                            "Failed to serialize node block task",
                        ));
                    }
                }
                final_error?;
                task_error?;
            }

            // Write out the block address list
            let block_hash_bytes = block_hash_bytes.freeze();
            let (list_address, _) = immutable::write(
                repository.clone(),
                Context::default(),
                block_hash_bytes.clone(),
                immutable::write_options_from_repository(repository.clone())
                    .with_local_cache_priority()
                    .with_max_size_chunk(),
            )
            .await
            .internal("Failed to serialize node block list")?;

            // Update the tree node block list address
            {
                lore_trace!(
                    "Update tree node block list from {} to {}",
                    tree.hash_node,
                    list_address.hash
                );
                tree.hash_node = list_address.hash;
                tree.flags |= TreeFlags::Dirty;
                {
                    let mut lock = self.runtime.write();
                    lock.tree = Some(tree);
                    lock.block_address = block_hash_bytes;
                }
            }
        }

        if !block_file_metadata_dirty.is_empty() {
            lore_trace!(
                "Serializing {} dirty file metadata blocks",
                block_file_metadata_dirty.len()
            );
            let mut tasks: JoinSet<Result<(Address, usize), StateError>> = JoinSet::new();
            for (block, block_index) in block_file_metadata_dirty.iter() {
                let block = block.clone();
                let block_index = *block_index;
                let repository = repository.clone();
                lore_trace!(
                    "Queue serialization of dirty file metadata node block {}",
                    block_index
                );

                lore_spawn!(tasks, async move {
                    lore_trace!("Serializing dirty file metadata node block {}", block_index);
                    // TODO(mjansson): Figure out a way to write the node block without having to copy
                    // it out of the lock first. Writing from the locked ref will not work as the immutable
                    // write makes the lock held of an await point
                    let mut node_block = { *block.read().node_block() };
                    node_block.flags &= !NodeBlockFlags::Dirty;
                    let (address, _) = node_block
                        .write_to_immutable(
                            repository.clone(),
                            Context::default(),
                            immutable::write_options_from_repository(repository.clone())
                                .with_local_cache_priority()
                                .with_max_size_chunk(),
                        )
                        .await
                        .internal("Failed to serialize file metadata block")?;
                    Ok((address, block_index))
                });
            }

            let mut block_hash_bytes = {
                let lock = self.runtime.read();
                // Resize buffer with empty hashes if needed
                lock.block_file_metadata_address
                    .clone_and_resize_zeroed::<Hash>(block_count)
            };
            {
                let block_hash = block_hash_bytes.as_type_slice_mut();

                let mut final_error = Ok(());
                let mut task_error = Ok(());
                while let Some(task) = tasks.join_next().await {
                    if let Ok(result) = task {
                        if let Ok((address, block_index)) = result {
                            block_hash[block_index] = address.hash;
                        } else {
                            final_error = Err(result.unwrap_err());
                        }
                    } else {
                        task_error = Err(StateError::internal_with_context(
                            task.unwrap_err(),
                            "Failed to serialize file metadata block task",
                        ));
                    }
                }
                final_error?;
                task_error?;
            }

            // Write out the block address list
            let block_hash_bytes = block_hash_bytes.freeze();
            let (list_address, _) = immutable::write(
                repository.clone(),
                Context::default(),
                block_hash_bytes.clone(),
                immutable::write_options_from_repository(repository.clone())
                    .with_local_cache_priority()
                    .with_max_size_chunk(),
            )
            .await
            .internal("Failed to serialize file metadata block list")?;

            // Update the tree file metadata node block list address
            {
                lore_trace!(
                    "Update tree file metadata node block list from {} to {}",
                    tree.hash_file_metadata,
                    list_address.hash
                );
                tree.hash_file_metadata = list_address.hash;
                tree.flags |= TreeFlags::Dirty;
                {
                    let mut lock = self.runtime.write();
                    lock.tree = Some(tree);
                    lock.block_file_metadata_address = block_hash_bytes;
                }
            }
        }

        let link_list = { self.runtime.read().link_list.clone() };
        if let Some(link_list) = link_list {
            let list_hash = hash::hash_slice(link_list.as_bytes());
            if list_hash != self.data.read().hash_link {
                let rehashed_list = if link_list.is_empty() {
                    lore_debug!("Link list empty, write default hash");
                    Hash::default()
                } else {
                    let bytes = Bytes::copy_from_slice(link_list.as_bytes());
                    let (address, _fragment) = immutable::write(
                        repository.clone(),
                        Context::default(),
                        bytes,
                        immutable::write_options_from_repository(repository.clone())
                            .with_local_cache_priority()
                            .with_max_size_chunk(),
                    )
                    .await
                    .internal("Failed to serialize link list")?;

                    address.hash
                };

                lore_debug!("Serialized link list to {rehashed_list}");
                let mut data = self.data.write();
                data.hash_link = rehashed_list;
                data.flags |= StateFlags::Dirty;
            }
        }

        // Serialize the immutable tree
        let tree = { self.runtime.read().tree.unwrap_or_default() };
        if tree.flags & TreeFlags::Dirty != 0 {
            lore_trace!("Serializing dirty tree");
            let (address, _fragment) = tree
                .write_to_immutable(
                    repository.clone(),
                    Context::default(),
                    immutable::write_options_from_repository(repository.clone())
                        .with_local_cache_priority()
                        .with_max_size_chunk(),
                )
                .await
                .internal("Failed to serialize tree")?;
            {
                lore_trace!("Serialized tree to {}", address.hash);
                lore_trace!("  node block {}", tree.hash_node);
                lore_trace!("  file metadata block {}", tree.hash_file_metadata);
                let mut data = self.data.write();
                data.hash_tree = address.hash;
                data.flags |= StateFlags::Dirty;
            }
        }

        // Serialize the state
        let (address, fragment) = {
            let buffer = {
                let mut data = self.data.write();
                data.flags &= !StateFlags::Dirty;
                data.format = StateFormat::LowerCaseHash as u32;
                data.magic = STATE_MAGIC;

                Bytes::copy_from_slice(data.as_bytes())
            };

            immutable::write(
                repository.clone(),
                Context::default(),
                buffer,
                immutable::write_options_from_repository(repository.clone())
                    .with_revision_state()
                    .with_local_cache_priority()
                    .with_max_size_chunk(),
            )
            .await
            .internal("Failed to serialize state")?
        };

        {
            let mut runtime = self.runtime.write();
            runtime.signature = address.hash;
        }

        lore_trace!(
            "Serialized state to {} in repository {}, {} -> {} bytes",
            address.hash,
            repository.id,
            fragment.size_content,
            fragment.size_payload
        );

        Ok(address.hash)
    }

    pub fn format(&self) -> u32 {
        self.data.read().format
    }

    pub fn flags(&self) -> u32 {
        self.data.read().flags
    }

    pub async fn update_tree_root_hash(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<(), StateError> {
        let root_block = {
            let runtime = self.runtime.read();
            if !runtime.block.is_empty() {
                runtime.block[0].upgrade()
            } else {
                None
            }
        };

        let root_data = {
            if let Some(root_block) = root_block {
                let mut block = root_block.write();
                let root_node = block.node(0);
                let root_hash = root_node.address.hash;
                let size = root_node.size;

                // By always resetting root node hash and size to zero we avoid first block
                // updating for every revision - if the updated subtree is fully
                // contained in another block(s) it should not affect the first block.
                root_node.address.hash.zero();
                root_node.size = 0;

                Some((root_hash, size))
            } else {
                None
            }
        };
        lore_trace!("Merkle tree root data {:?}", root_data);

        let tree = {
            if let Some((root_hash, size)) = root_data {
                let mut tree = self.tree(repository.clone()).await?;
                if root_hash != tree.hash_root {
                    tree.hash_root = root_hash;
                    tree.size = size;
                    tree.flags |= TreeFlags::Dirty;
                    Some(tree)
                } else {
                    None
                }
            } else {
                None
            }
        };

        let dirty = {
            if let Some(tree) = tree {
                let mut runtime = self.runtime.write();
                runtime.tree = Some(tree);
                true
            } else {
                false
            }
        };

        if dirty {
            let mut data = self.data.write();
            data.flags |= StateFlags::Dirty;
        }

        Ok(())
    }

    pub async fn branch(&self, repository: Arc<RepositoryContext>) -> Context {
        let metadata = self.metadata_hash();
        let metadata = metadata::Metadata::deserialize(repository, metadata)
            .await
            .internal("Failed to deserialize metadata")
            .unwrap_or_default();
        metadata.get_branch().unwrap_or_default()
    }

    pub fn metadata_hash(&self) -> Hash {
        self.data.read().hash_metadata
    }

    pub fn set_metadata_hash(&self, metadata: Hash) {
        let mut data = self.data.write();
        data.hash_metadata = metadata;
        data.flags |= StateFlags::Dirty;
    }

    pub fn set_delta_block(&self, delta_block: Hash, delta_count: usize) -> Result<(), StateError> {
        let mut tree = self.tree_readonly()?;
        if tree.hash_delta != delta_block {
            tree.hash_delta = delta_block;
            tree.delta_count = delta_count as u32;
            tree.flags |= TreeFlags::Dirty;
            self.runtime.write().tree = Some(tree);
        }
        Ok(())
    }

    pub async fn delta_block(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Bytes, StateError> {
        let tree = self.tree(repository.clone()).await?;
        let options = immutable::read_options_from_repository(&repository)
            .with_cache()
            .with_priority();
        Ok(immutable::read(
            repository,
            Address::zero_context_hash(tree.hash_delta),
            None, /* Full range */
            options,
        )
        .await
        .internal("Failed to deserialize delta block")?)
    }

    pub async fn node_delta(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Option<NodeDelta>, StateError> {
        let delta_block = self
            .delta_block(repository.clone())
            .await?
            .to_aligned::<NodeDelta>();

        for node_delta in delta_block.as_type_slice::<NodeDelta>().iter() {
            if node_delta.node == node {
                return Ok(Some(*node_delta));
            }
        }

        Ok(None)
    }

    pub fn block_count(&self) -> usize {
        let runtime = self.runtime.read();
        let mut block_count = runtime.block.len();
        if let Some(tree) = runtime.tree
            && tree.block_count > block_count as u32
        {
            block_count = tree.block_count as usize;
        }
        block_count
    }

    pub async fn block(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeBlock>, StateError> {
        {
            let lock = self.runtime.read();
            if lock.block.len() > block_index
                && let Some(block) = lock.block[block_index].upgrade()
            {
                return Ok(block);
            }
        }

        Box::pin(async move { self.block_deserialize(repository, block_index).await }).await
    }

    pub async fn try_block(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Option<Arc<NodeBlock>> {
        {
            let lock = self.runtime.read();
            if lock.block.len() > block_index
                && let Some(block) = lock.block[block_index].upgrade()
            {
                return Some(block);
            }
        }

        Box::pin(async move { self.try_block_deserialize(repository, block_index).await }).await
    }

    async fn try_block_deserialize(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Option<Arc<NodeBlock>> {
        let (block_count, _hash_node) = {
            let Ok(tree) = self.tree(repository.clone()).await else {
                return None;
            };
            (tree.block_count as usize, tree.hash_node)
        };
        if block_index >= block_count {
            return None;
        }

        self.block_deserialize(repository, block_index).await.ok()
    }

    async fn block_deserialize(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeBlock>, StateError> {
        let (block_count, hash_node) = {
            let tree = self.tree(repository.clone()).await?;
            (tree.block_count as usize, tree.hash_node)
        };
        if block_index >= block_count {
            return Err(StateError::internal(format!(
                "Invalid block index: {block_index}"
            )));
        }

        let (mut block_hash_bytes, rehash_node_names) = {
            let lock = self.runtime.read();
            (lock.block_address.clone(), lock.rehash_node_names)
        };

        if block_index >= block_hash_bytes.count::<Hash>() {
            // Avoid multiple tasks deserializing block list and block at the same time
            let _guard = self
                .block_deserialize
                .acquire()
                .await
                .internal("Failed to deserialize node block")?;

            block_hash_bytes = {
                let lock = self.runtime.read();
                lock.block_address.clone()
            };

            // Check if deserialize still needed after getting lock
            if block_index >= block_hash_bytes.count::<Hash>() {
                if hash_node.is_zero() {
                    let block = Arc::new(NodeBlock::new_zeroed());
                    if block_index == 0 {
                        // Reserve root node
                        let block = block.clone();
                        let mut block_writer = block.write();
                        let node_block = block_writer.node_block();
                        node_block.node_count = 1;
                    }
                    {
                        let mut lock = self.runtime.write();
                        if block_index >= lock.block.len() {
                            lock.block.resize(block_count, Weak::default());
                        }
                        if let Some(prev_block) = lock.block[block_index].upgrade() {
                            return Ok(prev_block);
                        }
                        lock.block[block_index] = Arc::downgrade(&block);
                    }
                    return Ok(block);
                }

                // TODO(mjansson): To support huge trees we might want to selectively
                // read the block addresses instead of all in one big buffer
                lore_trace!("Deserialize block address list");
                let address = Address::zero_context_hash(hash_node);
                block_hash_bytes = immutable::read(
                    repository.clone(),
                    address,
                    None, /* Read the full array of block hashes */
                    immutable::read_options_from_repository(&repository)
                        .with_cache()
                        .with_priority(),
                )
                .await
                .forward::<StateError>("Failed to deserialize node block list")?;
                if block_hash_bytes.count::<Hash>() < block_count {
                    block_hash_bytes = block_hash_bytes
                        .clone_and_resize_zeroed::<Hash>(block_count)
                        .freeze();
                }
                {
                    self.runtime.write().block_address = block_hash_bytes.clone();
                }
                lore_trace!("Deserialized block address list");
            }
        }

        let block_hash = block_hash_bytes.as_type_slice::<Hash>();
        if block_index >= block_hash.len() {
            return Err(StateError::internal(format!(
                "Invalid block index: {block_index}"
            )));
        }

        if block_hash[block_index].is_zero() {
            let block = Arc::new(NodeBlock::new_zeroed());
            let mut lock = self.runtime.write();
            if block_index >= lock.block.len() {
                lock.block.resize(block_count, Weak::default());
            }
            if let Some(prev_block) = lock.block[block_index].upgrade() {
                return Ok(prev_block);
            }
            if block_index == 0 {
                // Reserve root node
                let block = block.clone();
                let mut block_writer = block.write();
                let node_block = block_writer.node_block();
                node_block.node_count = 1;
            }
            lock.block[block_index] = Arc::downgrade(&block);
            if lock.block_cache.len() < MAX_BLOCK_CACHE {
                lock.block_cache.push(block.clone());
            } else {
                let cache_index = lock
                    .block_cache_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    as usize;
                lock.block_cache[cache_index % MAX_BLOCK_CACHE] = block.clone();
            }
            return Ok(block);
        }

        lore_trace!("Deserialize state node block {block_index}");

        // Deserialize the node file metadata block as well to force it to be cached in local store
        let metadata_block_cache = self
            .block_file_metadata_cache(repository.clone(), block_index)
            .await;

        let address = Address::zero_context_hash(block_hash[block_index]);
        let result = match NodeBlock::deserialize(repository.clone(), self, address).await {
            Ok(block) => {
                let block = Arc::new(block);
                {
                    let mut lock = self.runtime.write();
                    if block_index >= lock.block.len() {
                        lock.block.resize(block_count, Weak::default());
                    }
                    if let Some(prev_block) = lock.block[block_index].upgrade() {
                        return Ok(prev_block);
                    }

                    if rehash_node_names {
                        lock.block_dirty.push((block.clone(), block_index));
                    }

                    lock.block[block_index] = Arc::downgrade(&block);
                    if lock.block_cache.len() < MAX_BLOCK_CACHE {
                        lock.block_cache.push(block.clone());
                    } else {
                        let cache_index = lock
                            .block_cache_counter
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            as usize;
                        lock.block_cache[cache_index % MAX_BLOCK_CACHE] = block.clone();
                    }
                }
                lore_trace!("Deserialized state node block {block_index}");

                Ok(block)
            }
            Err(err) => Err(err),
        };

        if let Some(cache_task) = metadata_block_cache {
            let _ = cache_task.await;
        }

        result
    }

    pub async fn block_with_nametable(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeBlock>, StateError> {
        let block = self.block(repository.clone(), block_index).await?;

        block.deserialize_nametable(repository).await?;

        Ok(block)
    }

    async fn block_file_metadata_cache(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Option<JoinHandle<()>> {
        let (tree, mut block_hash_bytes) = {
            let lock = self.runtime.read();
            if lock.block_file_metadata.len() > block_index
                && lock.block_file_metadata[block_index].upgrade().is_some()
            {
                return None;
            }
            (lock.tree?, lock.block_file_metadata_address.clone())
        };

        if block_index >= block_hash_bytes.count::<Hash>() {
            if tree.hash_file_metadata.is_zero() {
                return None;
            }

            // TODO(mjansson): To support huge trees we might want to selectively
            // read the block addresses instead of all in one big buffer
            let address = Address::zero_context_hash(tree.hash_file_metadata);
            let Ok(hash_bytes) = immutable::read(
                repository.clone(),
                address,
                None, /* Read the full array of block hashes */
                immutable::read_options_from_repository(&repository)
                    .with_cache()
                    .with_priority(),
            )
            .await
            else {
                return None;
            };
            block_hash_bytes = hash_bytes;
            if block_hash_bytes.count::<Hash>() < tree.block_count as usize {
                block_hash_bytes = block_hash_bytes
                    .clone_and_resize_zeroed::<Hash>(tree.block_count as usize)
                    .freeze();
            }
            {
                self.runtime.write().block_file_metadata_address = block_hash_bytes.clone();
            }
        }

        let block_hash = block_hash_bytes.as_type_slice::<Hash>();
        if block_hash[block_index].is_zero() {
            return None;
        }

        let address = Address::zero_context_hash(block_hash[block_index]);
        Some(lore_spawn!(async move {
            let matched = repository
                .immutable_store()
                .query(repository.id, address, StoreMatch::MatchHash)
                .await
                .map_or(StoreMatch::MatchNone, |result| result.match_made);
            if matched == StoreMatch::MatchNone {
                let _ = immutable::read(
                    repository.clone(),
                    address,
                    None,
                    immutable::read_options_from_repository(&repository)
                        .with_cache()
                        .with_priority(),
                )
                .await;
            }
        }))
    }

    pub async fn block_file_metadata(
        &self,
        repository: Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<Arc<NodeFileMetadataBlock>, StateError> {
        {
            let lock = self.runtime.read();
            if lock.block_file_metadata.len() > block_index
                && let Some(block) = lock.block_file_metadata[block_index].upgrade()
            {
                return Ok(block);
            }
        }

        let tree = self.tree(repository.clone()).await?;

        let mut block_hash_bytes = {
            let mut lock = self.runtime.write();
            if lock.block_file_metadata.len() > block_index
                && let Some(block) = lock.block_file_metadata[block_index].upgrade()
            {
                return Ok(block);
            }

            if block_index >= tree.block_count as usize {
                return Err(StateError::internal(format!(
                    "Invalid block index: {block_index}"
                )));
            }
            if block_index >= lock.block_file_metadata.len() {
                lock.block_file_metadata
                    .resize(tree.block_count as usize, Weak::default());
            }

            lock.block_file_metadata_address.clone()
        };
        // At this point block_index is guaranteed to be < block.len()

        if block_index >= block_hash_bytes.count::<Hash>() {
            let _guard = self
                .metadata_deserialize
                .acquire()
                .await
                .internal("Failed to deserialize metadata")?;

            block_hash_bytes = {
                let lock = self.runtime.read();
                lock.block_file_metadata_address.clone()
            };

            if block_index >= block_hash_bytes.count::<Hash>() {
                if tree.hash_file_metadata.is_zero() {
                    let block = Arc::new(NodeFileMetadataBlock::default());
                    {
                        let mut lock = self.runtime.write();
                        if let Some(prev_block) = lock.block_file_metadata[block_index].upgrade() {
                            return Ok(prev_block);
                        }
                        lock.block_file_metadata[block_index] = Arc::downgrade(&block);
                    }
                    return Ok(block);
                }

                // TODO(mjansson): To support huge trees we might want to selectively
                // read the block addresses instead of all in one big buffer
                let address = Address::zero_context_hash(tree.hash_file_metadata);
                block_hash_bytes = immutable::read(
                    repository.clone(),
                    address,
                    None, /* Read the full array of block hashes */
                    immutable::read_options_from_repository(&repository)
                        .with_cache()
                        .with_priority(),
                )
                .await
                .forward::<StateError>("Failed to deserialize node block list")?;
                if block_hash_bytes.count::<Hash>() < tree.block_count as usize {
                    block_hash_bytes = block_hash_bytes
                        .clone_and_resize_zeroed::<Hash>(tree.block_count as usize)
                        .freeze();
                }
                {
                    self.runtime.write().block_file_metadata_address = block_hash_bytes.clone();
                }
            }
        }

        let block_hash = block_hash_bytes.as_type_slice::<Hash>();
        if block_hash[block_index].is_zero() {
            let block = Arc::new(NodeFileMetadataBlock::default());
            let mut lock = self.runtime.write();
            if let Some(prev_block) = lock.block_file_metadata[block_index].upgrade() {
                return Ok(prev_block);
            }
            lock.block_file_metadata[block_index] = Arc::downgrade(&block);
            return Ok(block);
        }

        let address = Address::zero_context_hash(block_hash[block_index]);
        let block = Arc::new({
            let mut block_data = NodeFileMetadataBlockData::read_box_from_immutable_compat(
                repository.clone(),
                address,
                true,
            )
            .await
            .internal("Failed to deserialize file metadata block")?;
            block_data.flags &= !NodeBlockFlags::Dirty;
            NodeFileMetadataBlock::new(block_data)
        });

        {
            let mut lock = self.runtime.write();
            if let Some(prev_block) = lock.block_file_metadata[block_index].upgrade() {
                return Ok(prev_block);
            }
            lock.block_file_metadata[block_index] = Arc::downgrade(&block);
        }
        Ok(block)
    }

    pub fn parents(&self) -> [Hash; 2] {
        self.data.read().parent
    }

    pub fn parent_self(&self) -> Hash {
        self.data.read().parent[0]
    }

    pub fn parent_other(&self) -> Hash {
        self.data.read().parent[1]
    }

    pub fn set_parent_self(&self, signature: Hash) {
        let mut lock = self.data.write();
        lock.parent[0] = signature;
        lock.flags |= StateFlags::Dirty;
    }

    pub fn set_parent_other(&self, signature: Hash) {
        let mut lock = self.data.write();
        lock.parent[1] = signature;
        if !signature.is_zero() {
            lock.flags |= StateFlags::Merge;
        } else {
            lock.flags &= !StateFlags::Merge;
        }
        lock.flags |= StateFlags::Dirty;
    }

    pub fn revision(&self) -> Hash {
        self.runtime.read().signature
    }

    pub fn state_data(&self) -> StateData {
        *self.data.read()
    }

    pub fn revision_number(&self) -> u64 {
        self.data.read().revision_number
    }

    pub fn set_revision_number(&self, revision_number: u64) {
        let mut data = self.data.write();
        if data.revision_number != revision_number {
            data.revision_number = revision_number;
            data.flags |= StateFlags::Dirty;
        }
    }

    pub fn is_merge(&self) -> bool {
        self.data.read().flags & StateFlags::Merge != 0
    }

    pub fn is_cherry_pick(&self) -> bool {
        self.data.read().flags & StateFlags::CherryPick != 0
    }

    pub fn is_revert(&self) -> bool {
        self.data.read().flags & StateFlags::Revert != 0
    }

    pub fn is_merge_or_cherry_pick_or_revert(&self) -> bool {
        self.data.read().flags & (StateFlags::Merge | StateFlags::CherryPick | StateFlags::Revert)
            != 0
    }

    pub fn is_conflict(&self) -> bool {
        self.data.read().flags & StateFlags::Conflict != 0
    }

    pub fn is_dirty(&self) -> bool {
        self.data.read().flags & StateFlags::Dirty != 0
    }

    pub fn set_merge(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Merge;
    }

    pub fn set_cherry_pick(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::CherryPick;
    }

    pub fn set_revert(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Revert;
    }

    pub fn set_conflict(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Conflict;
        data.flags |= StateFlags::Dirty;
    }

    pub fn set_merge_conflict(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Conflict | StateFlags::Merge | StateFlags::Dirty;
    }

    pub fn set_cherry_pick_conflict(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Conflict | StateFlags::CherryPick | StateFlags::Dirty;
    }

    pub fn set_revert_conflict(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Conflict | StateFlags::Revert | StateFlags::Dirty;
    }

    pub fn reset_merge_conflict_flags(&self) {
        let mut data = self.data.write();
        data.flags &= !(StateFlags::Conflict
            | StateFlags::Merge
            | StateFlags::CherryPick
            | StateFlags::Revert);
    }

    pub fn block_modified(&self, block: Arc<NodeBlock>, index: usize) {
        let mut lock = self.runtime.write();
        for tuple in lock.block_dirty.iter() {
            if tuple.1 == index {
                return;
            }
        }
        lore_trace!("Node block {index} marked dirty");
        lock.block_dirty.push((block.clone(), index));
    }

    pub fn block_file_metadata_modified(&self, block: Arc<NodeFileMetadataBlock>, index: usize) {
        let mut lock = self.runtime.write();
        for tuple in lock.block_file_metadata_dirty.iter() {
            if tuple.1 == index {
                //panic!("File metadata block marked as modified twice");
                return;
            }
        }
        lore_trace!("File metadata block {index} marked dirty");
        lock.block_file_metadata_dirty.push((block.clone(), index));
    }

    pub fn mark_dirty(&self) {
        let mut data = self.data.write();
        data.flags |= StateFlags::Dirty;
    }

    pub async fn node_add(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> Result<NodeID, StateError> {
        let permit = self.unused.acquire().await;

        let mut node_id = INVALID_NODE;
        let tree = self.tree(repository.clone()).await?;
        let mut block_index = tree.block_unused_first as usize;
        let block_count = self.block_count();
        lore_trace!("node_add block unused {block_index} block count {block_count}");
        while block_index < block_count && !node_id.is_valid_node_id() {
            let block = self.block(repository.clone(), block_index).await?;
            let (dirtied, block_full, next_unused_index) = {
                let mut block = block.write();
                let next_unused_index = block.block_unused_next();
                node_id = block.grab_node_unused(block_index as u32);
                if node_id.is_valid_node_id() {
                    (block.mark_dirty(), block.is_full(), next_unused_index)
                } else {
                    (false, true, next_unused_index)
                }
            };
            if dirtied {
                self.block_modified(block.clone(), block_index);
                self.mark_dirty();
            }
            if block_full {
                let mut popped_dirty = false;
                {
                    let mut runtime = self.runtime.write();
                    if let Some(tree) = runtime.tree.as_mut()
                        && tree.block_unused_first == block_index as u32
                    {
                        tree.block_unused_first = next_unused_index;
                        tree.flags |= TreeFlags::Dirty;
                        let mut block_writer = block.write();
                        block_writer.node_block().block_unused_next = INVALID_BLOCK;
                        popped_dirty = block_writer.mark_dirty();
                    }
                }
                if popped_dirty {
                    self.block_modified(block.clone(), block_index);
                    self.mark_dirty();
                }
            }
            if !node_id.is_valid_node_id() {
                if block_index != next_unused_index as usize {
                    block_index = next_unused_index as usize;
                } else {
                    block_index = INVALID_BLOCK as usize;
                }
            }
        }

        if !node_id.is_valid_node_id()
            && let Some((idx, block)) = self.try_recycle_last_block()
        {
            let candidate = {
                let mut block_writer = block.write();
                let id = block_writer.grab_node_unused(idx as u32);
                if id.is_valid_node_id() {
                    block_writer.mark_dirty();
                }
                id
            };
            if candidate.is_valid_node_id() {
                node_id = candidate;
                self.block_modified(block.clone(), idx);
                self.mark_dirty();
                self.push_unused_block_list(idx, &block);
            }
        }

        if !node_id.is_valid_node_id() {
            let (idx, block) = self.allocate_fresh_block()?;
            node_id = {
                let mut block_writer = block.write();
                let id = block_writer.grab_node_unused(idx as u32);
                if id.is_valid_node_id() {
                    block_writer.mark_dirty();
                }
                id
            };
            if !node_id.is_valid_node_id() {
                return Err(StateError::internal(
                    "grab_node_unused returned INVALID on a freshly-allocated block",
                ));
            }
            self.block_modified(block.clone(), idx);
            self.mark_dirty();
        }

        drop(permit);

        let block_index = NodeBlock::index(node_id);
        lore_trace!("Block {} node {} added", block_index, Node::index(node_id));
        let parent_block_index = NodeBlock::index(parent);
        let parent_block = self.block(repository.clone(), parent_block_index).await?;
        let (dirtied, sibling) = {
            let mut parent_lock = parent_block.write();
            let parent_node = parent_lock.node(Node::index(parent));
            let sibling = parent_node.child;
            parent_node.child = node_id;
            (parent_lock.mark_dirty(), sibling)
        };
        if dirtied {
            self.block_modified(parent_block, parent_block_index);
        }

        lore_trace!(
            "Block {} node {} parent {} sibling {}",
            block_index,
            Node::index(node_id),
            parent,
            sibling
        );

        let block = self
            .block_with_nametable(repository.clone(), block_index)
            .await?;
        let dirtied = {
            let mut block_lock = block.write();
            let (name_offset, name_length) = block_lock
                .node_name_store(name, 0, 0)
                .forward::<StateError>("Storing new node name")?;
            let target_node = block_lock.node(Node::index(node_id));
            *target_node = node;
            target_node.parent = parent;
            target_node.sibling = sibling;
            target_node.name_offset = name_offset;
            target_node.name_length = name_length;
            block_lock.mark_dirty()
        };
        if dirtied {
            self.block_modified(block, block_index);
        }

        let metadata_node_id = node::node_to_file_metadata(node_id);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node_id);
        let metadata_node_index = NodeFileMetadata::index(metadata_node_id);

        let metadata_block = self
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await?;

        let dirtied = {
            let mut block_lock = metadata_block.write();

            let node_metadata = block_lock.node(metadata_node_index);
            if !node_metadata.metadata.is_zero() {
                node_metadata.metadata.zero();

                block_lock.mark_dirty()
            } else {
                false
            }
        };

        if dirtied {
            self.block_file_metadata_modified(metadata_block, metadata_block_index);
        }

        Ok(node_id)
    }

    /// Return the most recently allocated block when it still has at least one
    /// free slot, without touching the unused chain. The caller is expected to
    /// attempt a grab before deciding whether to splice the block into the
    /// chain — so a block whose internal bookkeeping diverges from `is_full()`
    /// is not introduced into the chain where it could mislead future scans.
    fn try_recycle_last_block(&self) -> Option<(usize, Arc<NodeBlock>)> {
        let runtime = self.runtime.read();
        let block_index = runtime.block.len().checked_sub(1)?;
        let block = runtime.block[block_index].upgrade()?;
        if block.read().is_full() {
            return None;
        }
        Some((block_index, block))
    }

    /// Allocate a fresh `NodeBlock`, push it onto the runtime's block vector
    /// and splice it at the head of the unused chain. Errors only when the
    /// per-tree block limit is reached. The returned block is guaranteed to
    /// have at least one free slot — a newly-zeroed block has
    /// `node_count == 0`, well below `BLOCK_NODE_COUNT` — so the caller's
    /// grab is structurally guaranteed to succeed.
    fn allocate_fresh_block(&self) -> Result<(usize, Arc<NodeBlock>), StateError> {
        let mut runtime = self.runtime.write();
        let block_index = runtime.block.len();
        if block_index >= MAX_TREE_BLOCK_COUNT as usize {
            return Err(StateError::from(Oversized {
                context: format!("tree block count limit reached: {MAX_TREE_BLOCK_COUNT}"),
            }));
        }
        let block = Arc::new(NodeBlock::new_zeroed());
        if block_index == 0 {
            let mut block_writer = block.write();
            block_writer.node_block().node_count = 1;
        }
        runtime.block.push(Arc::downgrade(&block));

        let prior_head = if let Some(tree) = runtime.tree.as_mut() {
            let prior = tree.block_unused_first;
            tree.block_count = 1 + block_index as u32;
            tree.block_unused_first = block_index as u32;
            tree.flags |= TreeFlags::Dirty;
            prior
        } else {
            INVALID_BLOCK
        };
        {
            let mut block_writer = block.write();
            block_writer.node_block().block_unused_next = prior_head;
            block_writer.mark_dirty();
        }
        drop(runtime);
        self.block_modified(block.clone(), block_index);
        Ok((block_index, block))
    }

    /// Insert `block` at the head of the unused chain when it isn't already
    /// there. The chain is a prepend-only singly-linked list whose head
    /// `tree.block_unused_first` advances only as exhausted blocks are popped;
    /// the most recently allocated block is therefore either at the head or
    /// not in the chain at all, so a non-head index here means the block can
    /// be safely linked at the front without traversing to deduplicate.
    fn push_unused_block_list(&self, block_index: usize, block: &Arc<NodeBlock>) {
        let block_idx_u32 = block_index as u32;
        let mut runtime = self.runtime.write();
        let Some(tree) = runtime.tree.as_mut() else {
            return;
        };
        if tree.block_unused_first == block_idx_u32 {
            return;
        }
        let prior_head = tree.block_unused_first;
        tree.block_unused_first = block_idx_u32;
        tree.flags |= TreeFlags::Dirty;
        let newly_dirty = {
            let mut block_writer = block.write();
            block_writer.node_block().block_unused_next = prior_head;
            block_writer.mark_dirty()
        };
        drop(runtime);
        if newly_dirty {
            self.block_modified(block.clone(), block_index);
        }
    }

    pub async fn node_children(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Vec<NodeID>, StateError> {
        let parent_id = node;
        let node = self.node(repository.clone(), node).await?;
        if node.is_directory() {
            let mut children = vec![];
            let mut child_node = node.child();
            let mut cycle = SiblingCycleGuard::new(parent_id);
            while let Some(child_id) = child_node {
                let child = self.node(repository.clone(), child_id).await?;
                child.walk_step(child_id, parent_id, &mut cycle)?;
                children.push(child_id);
                child_node = child.sibling();
            }
            Ok(children)
        } else if node.is_link() {
            let link = node.linked_node();
            let linked_repository = Arc::new(repository.to_link_context(link.repository).await);
            let link_state = State::deserialize(linked_repository.clone(), link.revision).await?;
            Box::pin(link_state.node_children(linked_repository.clone(), link.node)).await
        } else {
            Ok(vec![])
        }
    }

    pub async fn node_name_clone(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<String, StateError> {
        let block = self
            .block_with_nametable(repository.clone(), NodeBlock::index(node))
            .await?;
        block
            .node_name_clone(Node::index(node))
            .internal("Node name")
            .map_err(StateError::from)
    }

    pub async fn node_name_ref(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<NodeNameLock, StateError> {
        let block = self
            .block_with_nametable(repository.clone(), NodeBlock::index(node))
            .await?;
        block
            .node_name_ref(Node::index(node))
            .internal("Node name")
            .map_err(StateError::from)
    }

    pub async fn node_mark(
        &self,
        repository: Arc<RepositoryContext>,
        mut node_id: NodeID,
        mut flags: NodeFlags,
        mut mark_dirty: bool,
    ) -> Result<(), StateError> {
        while node_id.is_valid_node_id() {
            let block_index = NodeBlock::index(node_id);
            let node_index = Node::index(node_id);
            let block = self.block(repository.clone(), block_index).await?;
            let (parent_id, dirtied) = {
                let mut locked_block = block.write();
                let node_block = locked_block.node_block();
                let node = &mut node_block.node[node_index];
                if !mark_dirty && (node.flags & flags) == flags {
                    lore_trace!("Node {} already marked with flags {:x}", node_id, flags);
                    return Ok(());
                }
                // The merge flag must always be maintained (unless explicitly dropped through unstaging)
                if node.is_staged_merge() {
                    flags |= NodeFlags::StagedMerge;
                }
                // The conflict flag must always be maintained (unless explicitly dropped through unstaging)
                if node.is_staged_merge_conflict() {
                    flags |= NodeFlags::StagedMergeConflict;
                }
                node.flags &= !NodeFlags::StagedBits;
                node.flags |= (NodeFlags::Staged | flags) & NodeFlags::StagedBits;
                lore_trace!(
                    "Node {} with parent {} now marked with flags {:x}",
                    node_id,
                    node.parent,
                    node.flags
                );
                (node.parent, locked_block.mark_dirty())
            };
            if dirtied {
                lore_trace!("Block {block_index} and state marked dirty");
                self.block_modified(block, block_index);
                self.mark_dirty();
            }

            mark_dirty = false;
            flags = NodeFlags::Staged;

            node_id = parent_id;
        }

        // If we get here the root block should be marked as dirty as this was the fist traversal
        // up to the root for the given subtree being walked
        let block = self.block(repository, 0).await?;
        let dirtied = {
            let mut locked_block = block.write();
            locked_block.mark_dirty()
        };
        if dirtied {
            lore_trace!("Block 0 and state marked dirty");
            self.block_modified(block, 0);
            self.mark_dirty();
        }

        Ok(())
    }

    pub async fn node_has_staged_children(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> Result<bool, StateError> {
        let mut has_staged = false;

        // TODO(vri): UCS-15592 - Improve by iteratively walking children
        let children = self
            .node_children(repository.clone(), parent_node)
            .await
            .internal("Node not found")?;

        for &child in &children {
            if self
                .node(repository.clone(), child)
                .await
                .internal("Node not found")?
                .is_staged()
            {
                lore_trace!("Child node {child} is staged");
                has_staged = true;
                break;
            }
        }

        Ok(has_staged)
    }

    /// Mark a node as dirty and propagate the Dirty flag up to parent directories.
    /// The target node is marked with the given dirty flags (including action bits).
    /// Parent directories get only the base Dirty flag (bit 3, no action bits).
    /// Early-out if a parent already has Dirty set (when `mark_dirty` is false).
    pub async fn node_mark_dirty(
        &self,
        repository: Arc<RepositoryContext>,
        mut node_id: NodeID,
        mut flags: NodeFlags,
        mut mark_dirty: bool,
    ) -> Result<(), StateError> {
        while node_id.is_valid_node_id() {
            let block_index = NodeBlock::index(node_id);
            let node_index = Node::index(node_id);
            let block = self.block(repository.clone(), block_index).await?;
            let (parent_id, dirtied) = {
                let mut locked_block = block.write();
                let node_block = locked_block.node_block();
                let node = &mut node_block.node[node_index];
                if !mark_dirty && (node.flags & flags) == flags {
                    lore_trace!(
                        "Node {} already marked with dirty flags {:x}",
                        node_id,
                        flags
                    );
                    return Ok(());
                }
                // Clear existing dirty+action bits, then set new ones.
                // This replaces the previous action (latest wins). Staged and merge bits are preserved.
                node.flags &= !NodeFlags::DirtyBits;
                node.flags |= flags & NodeFlags::DirtyBits;
                lore_trace!(
                    "Node {} with parent {} now marked with dirty flags {:x}",
                    node_id,
                    node.parent,
                    node.flags
                );
                (node.parent, locked_block.mark_dirty())
            };
            if dirtied {
                lore_trace!("Block {block_index} and state marked dirty");
                self.block_modified(block, block_index);
                self.mark_dirty();
            }

            // For parent nodes, only set the base Dirty flag (no action bits)
            mark_dirty = false;
            flags = NodeFlags::Dirty;

            node_id = parent_id;
        }

        // Mark root block as dirty on first traversal up to root
        let block = self.block(repository, 0).await?;
        let dirtied = {
            let mut locked_block = block.write();
            locked_block.mark_dirty()
        };
        if dirtied {
            lore_trace!("Block 0 and state marked dirty");
            self.block_modified(block, 0);
            self.mark_dirty();
        }

        Ok(())
    }

    /// Check if a parent node has any children with the Dirty flag set.
    pub async fn node_has_dirty_children(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
    ) -> Result<bool, StateError> {
        let mut has_dirty = false;

        let children = self
            .node_children(repository.clone(), parent_node)
            .await
            .internal("Node not found")?;

        for &child in &children {
            if self
                .node(repository.clone(), child)
                .await
                .internal("Node not found")?
                .is_dirty()
            {
                lore_trace!("Child node {child} is dirty");
                has_dirty = true;
                break;
            }
        }

        Ok(has_dirty)
    }

    /// Clear the dirty flags on `node_id` and propagate the clear up the parent
    /// chain: each ancestor with no remaining dirty children also has its dirty
    /// flags cleared. Staged flags are preserved (see [`Node::clear_dirty_flags`]).
    ///
    /// This is the inverse of [`node_mark_dirty`](Self::node_mark_dirty) and is
    /// used both by the filesystem scan when a tracked file is found unmodified
    /// and by `status --check-dirty` when a dirty flag turns out to be stale.
    pub async fn node_clear_dirty(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
    ) -> Result<(), StateError> {
        if !node_id.is_valid_node_id() {
            return Ok(());
        }

        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = self.block(repository.clone(), block_index).await?;
        let mut parent_id = block.node(node_index).parent;
        let block_dirtied = {
            let mut locked_block = block.write();
            locked_block.node(node_index).clear_dirty_flags();
            locked_block.mark_dirty()
        };
        if block_dirtied {
            self.block_modified(block, block_index);
            self.mark_dirty();
        }

        while parent_id.is_valid_node_id() {
            if self
                .node_has_dirty_children(repository.clone(), parent_id)
                .await?
            {
                break;
            }
            let parent_block_index = NodeBlock::index(parent_id);
            let parent_node_index = Node::index(parent_id);
            let parent_block = self.block(repository.clone(), parent_block_index).await?;
            let next_parent_id = parent_block.node(parent_node_index).parent;
            let parent_block_dirtied = {
                let mut locked_block = parent_block.write();
                locked_block.node(parent_node_index).clear_dirty_flags();
                locked_block.mark_dirty()
            };
            if parent_block_dirtied {
                self.block_modified(parent_block, parent_block_index);
                self.mark_dirty();
            }
            if parent_id == ROOT_NODE {
                break;
            }
            parent_id = next_parent_id;
        }

        Ok(())
    }

    /// Collect the repository-relative paths of all dirty nodes at or under
    /// `root_node`, the set to stage. `base_path` is `root_node`'s path and the
    /// prefix of every returned path.
    pub async fn collect_dirty_paths(
        &self,
        repository: Arc<RepositoryContext>,
        root_node: NodeID,
        base_path: RelativePathBuf,
    ) -> Result<Vec<RelativePath>, StateError> {
        let mut result = Vec::new();
        let force = execution_context().globals().force();
        // Work in immutable `RelativePath` throughout: `push_into_buf` borrows
        // the parent (no per-sibling clone) and a single `freeze` per retained
        // child is reused as the stored stack/result value, so the filter check
        // borrows it without an extra allocation.
        let mut stack: Vec<(NodeID, RelativePath)> = vec![(root_node, base_path.freeze())];

        while let Some((node_id, path)) = stack.pop() {
            let children = self
                .node_children(repository.clone(), node_id)
                .await
                .internal("Failed to get children for dirty path collection")?;

            if node_id == root_node && children.is_empty() && !path.is_empty() {
                let node = self.node(repository.clone(), node_id).await?;
                if node.is_dirty_add()
                    && node.is_directory()
                    && (force || !repository.filter.excludes(&path, true, FilterMode::Full))
                {
                    result.push(path);
                }
                continue;
            }

            for &child_id in &children {
                let child = self.node(repository.clone(), child_id).await?;
                if !child.is_dirty() {
                    continue;
                }

                let name = self.node_name_clone(repository.clone(), child_id).await?;
                let child_path = path.push_into_buf(&name).freeze();

                // Don't carry forward dirty paths the view/ignore filter
                // excludes; they can't be replayed against a checkout that
                // never materializes them. --force bypasses the filter.
                if !force
                    && repository.filter.excludes(
                        &child_path,
                        child.is_directory(),
                        FilterMode::Full,
                    )
                {
                    continue;
                }

                if child.is_file() {
                    result.push(child_path);
                } else if child.is_directory() {
                    // A new (dirty-add) directory is itself a change to stage; an
                    // empty one has no child files to carry it.
                    if child.is_dirty_add() {
                        result.push(child_path.clone());
                    }
                    // A dirty-delete directory is staged as the deletion of its
                    // whole subtree: stage_delete on a directory recurses and
                    // stages a delete for every descendant. Emit just this path
                    // and don't descend — collecting descendants here would only
                    // queue redundant deletes that stage_delete already covers.
                    if child.is_dirty_delete() {
                        result.push(child_path);
                    } else {
                        stack.push((child_id, child_path));
                    }
                }
            }
        }

        Ok(result)
    }

    pub async fn find_subnode(
        &self,
        repository: Arc<RepositoryContext>,
        parent_node: NodeID,
        name_hash: u64,
    ) -> Result<NodeID, StateError> {
        let mut iblock = NodeBlock::index(parent_node);
        let mut inode = Node::index(parent_node);
        let mut block = self.block(repository.clone(), iblock).await?;

        // TODO(mjansson): This does not actually need to grab the whole node
        let node = { *block.read().node(inode) };
        if !node.is_directory() {
            return Err(NodeNotFound.into());
        }

        let mut child_node_ref = node.child();
        let mut cycle = SiblingCycleGuard::new(parent_node);
        while let Some(node_id) = child_node_ref {
            let inextblock = NodeBlock::index(node_id);
            inode = Node::index(node_id);
            let node = {
                if iblock != inextblock {
                    iblock = inextblock;
                    block = self.block(repository.clone(), iblock).await?;
                }
                *block.read().node(inode)
            };

            node.walk_step(node_id, parent_node, &mut cycle)?;

            if node.name_hash == name_hash {
                return Ok(node_id);
            }

            child_node_ref = node.sibling();
        }

        Err(NodeNotFound.into())
    }

    pub async fn find_relative_node_link(
        &self,
        repository: Arc<RepositoryContext>,
        root: NodeID,
        path: &str,
    ) -> Result<NodeLink, StateError> {
        let mut path = RelativePath::from_str(path).unwrap();
        let mut current_node = root;
        let mut repository = repository;
        while !path.is_empty() {
            let current_name = path.pop_root();
            let name_hash = hash::hash_string(current_name);

            current_node = self
                .find_subnode(repository.clone(), current_node, name_hash)
                .await?;

            // If the node is a link, resolve and enter that link
            if !path.is_empty() {
                let iblock = NodeBlock::index(current_node);
                let inode = Node::index(current_node);
                let block = self.block(repository.clone(), iblock).await?;
                let node = block.node(inode);

                if node.is_link() {
                    let link = node.linked_node();
                    repository = Arc::new(repository.to_link_context(link.repository).await);
                    let link_state = State::deserialize(repository.clone(), link.revision).await?;
                    return Box::pin(link_state.find_relative_node_link(
                        repository,
                        link.node,
                        path.as_str(),
                    ))
                    .await;
                }
            }
        }

        Ok(NodeLink {
            node: current_node,
            repository: repository.id,
            revision: self.revision(),
        })
    }

    pub async fn find_node_link(
        &self,
        repository: Arc<RepositoryContext>,
        path: &str,
    ) -> Result<NodeLink, StateError> {
        self.find_relative_node_link(repository, ROOT_NODE, path)
            .await
    }

    pub async fn find_link_parent_node(
        &self,
        repository: Arc<RepositoryContext>,
        path: &str,
        target_repository_id: RepositoryId,
    ) -> Result<NodeID, StateError> {
        let mut current_path = RelativePath::from_str(path).unwrap_or_default();

        while !current_path.is_empty() {
            current_path.pop();

            if current_path.is_empty() {
                break;
            }

            if let Ok(node_link) = self
                .find_node_link(repository.clone(), current_path.as_str())
                .await
                && node_link.is_valid()
                && let Ok(node) = self.node(repository.clone(), node_link.node).await
                && node.is_link()
                && node.address.context == target_repository_id.into()
            {
                return Ok(node_link.node);
            }
        }

        Err(NodeNotFound.into())
    }

    pub async fn find_node(
        &self,
        repository: Arc<RepositoryContext>,
        path: &str,
    ) -> Result<Node, StateError> {
        let node_link = self.find_node_link(repository.clone(), path).await?;

        let iblock = NodeBlock::index(node_link.node);
        let inode = Node::index(node_link.node);
        if node_link.revision == self.revision() {
            let block = self.block(repository.clone(), iblock).await?;
            let block_reader = block.read();
            Ok(*block_reader.node(inode))
        } else {
            let repository = Arc::new(repository.to_link_context(node_link.repository).await);
            let state = State::deserialize(repository.clone(), node_link.revision).await?;
            let block = state.block(repository, iblock).await?;
            let block_reader = block.read();
            Ok(*block_reader.node(inode))
        }
    }

    pub async fn node(
        &self,
        repository: Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<Node, StateError> {
        if !node.is_valid_or_root_node_id() {
            return Err(StateError::internal("Invalid node"));
        }
        let iblock = NodeBlock::index(node);
        let inode = Node::index(node);
        let block = self.block(repository, iblock).await?;
        let block_reader = block.read();
        Ok(*block_reader.node(inode))
    }

    pub async fn try_node(&self, repository: Arc<RepositoryContext>, node: NodeID) -> Option<Node> {
        if !node.is_valid_or_root_node_id() {
            return None;
        }
        let iblock = NodeBlock::index(node);
        let inode = Node::index(node);
        if let Some(block) = self.try_block(repository, iblock).await {
            let block_reader = block.read();
            Some(*block_reader.node(inode))
        } else {
            None
        }
    }

    pub async fn node_path(
        &self,
        repository: Arc<RepositoryContext>,
        mut node: NodeID,
    ) -> Result<String, StateError> {
        if node == ROOT_NODE {
            return Ok(String::new());
        }

        let mut nodes = vec![];
        while node.is_valid_node_id() {
            nodes.push(node);

            let block_index = NodeBlock::index(node);
            let node_index = Node::index(node);
            let block = self.block(repository.clone(), block_index).await?;
            node = block.node(node_index).parent;
        }

        let mut path = RelativePathBuf::new();
        for node in nodes.iter().rev() {
            let name = self
                .node_name_ref(repository.clone(), *node)
                .await
                .internal("Node name")?;
            path.push(name);
        }

        Ok(path.to_string())
    }

    pub async fn collect_children_unsorted(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        include_deleted: bool,
        include_links: bool,
    ) -> Result<StateChildrenNodes, StateError> {
        let mut children = vec![];
        if !parent.is_valid_or_root_node_id() {
            return Ok(StateChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        let node = self.node(repository.clone(), parent).await?;
        if node.is_file() {
            return Ok(StateChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        if node.is_link() {
            if !include_links {
                return Ok(StateChildrenNodes {
                    repository,
                    state: self.clone(),
                    children,
                });
            }

            let link = node.linked_node();
            let linked_repository = link.repository;
            let signature = link.revision;
            let link_node = link.node;
            let linked_repository = Arc::new(repository.to_link_context(linked_repository).await);
            let link_state = State::deserialize(linked_repository.clone(), signature)
                .await
                .internal("Link error")?;

            let result = Box::pin(link_state.collect_children_unsorted(
                linked_repository.clone(),
                link_node,
                include_deleted,
                include_links,
            ))
            .await?;

            return Ok(result);
        }

        let mut iter =
            StateNodeChildrenIterator::new(self.clone(), repository.clone(), parent).await?;
        while let Some((child_id, child_node)) = iter.next().await? {
            if include_deleted || !child_node.is_staged_delete() {
                children.push(StateNamedNode {
                    node: child_id,
                    name: child_node.name_hash,
                });
            }
        }

        Ok(StateChildrenNodes {
            repository,
            state: self.clone(),
            children,
        })
    }

    pub async fn collect_named_children_unsorted(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        include_deleted: bool,
        include_links: bool,
    ) -> Result<StateNamedChildrenNodes, StateError> {
        let mut children = vec![];
        if !parent.is_valid_or_root_node_id() {
            return Ok(StateNamedChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        let node = self.node(repository.clone(), parent).await?;
        if node.is_file() {
            return Ok(StateNamedChildrenNodes {
                repository,
                state: self.clone(),
                children,
            });
        }

        if node.is_link() {
            if !include_links {
                return Ok(StateNamedChildrenNodes {
                    repository,
                    state: self.clone(),
                    children,
                });
            }

            let link = node.linked_node();
            let linked_repository = link.repository;
            let signature = link.revision;
            let link_node = link.node;
            let linked_repository = Arc::new(repository.to_link_context(linked_repository).await);
            let link_state = State::deserialize(linked_repository.clone(), signature)
                .await
                .internal("Link error")?;

            let result = Box::pin(link_state.collect_named_children_unsorted(
                linked_repository.clone(),
                link_node,
                include_deleted,
                include_links,
            ))
            .await?;

            return Ok(result);
        }

        let mut iter =
            StateNodeChildrenWithNameIterator::new(self.clone(), repository.clone(), parent)
                .await?;
        while let Some((child_id, child_node, name_lock)) = iter.next().await? {
            if include_deleted || !child_node.is_staged_delete() {
                children.push(StateNamedStringNode {
                    node: child_id,
                    name: child_node.name_hash,
                    name_string: name_lock.freeze(),
                });
            }
        }

        Ok(StateNamedChildrenNodes {
            repository,
            state: self.clone(),
            children,
        })
    }

    pub fn tree_readonly(&self) -> Result<Tree, StateError> {
        {
            let lock = self.runtime.read();
            if let Some(tree) = lock.tree.as_ref() {
                return Ok(*tree);
            }
        }
        Err(StateError::internal("Tree not loaded"))
    }

    pub async fn tree(&self, repository: Arc<RepositoryContext>) -> Result<Tree, StateError> {
        {
            let lock = self.runtime.read();
            if let Some(tree) = lock.tree.as_ref() {
                return Ok(*tree);
            }
        }

        let hash_tree = { self.data.read().hash_tree };

        let tree = {
            if hash_tree.is_zero() {
                let mut tree = Tree::new_zeroed();
                tree.magic = TREE_MAGIC;
                tree.format = TreeFormat::Initial as u32;
                tree.block_count = 1;
                {
                    let mut lock = self.runtime.write();
                    lock.tree = Some(tree);
                    lock.block.push(Weak::new());
                }
                tree
            } else {
                let tree_address = Address::zero_context_hash(hash_tree);
                let options = read_options_from_repository(&repository);
                let mut tree = Tree::read_from_immutable(repository, tree_address, options)
                    .await
                    .forward::<StateError>("Failed to deserialize tree")?;
                if tree.magic != TREE_MAGIC {
                    return Err(StateError::internal("Tree corrupt header"));
                } else if tree.format == 0 || tree.format > TreeFormat::Initial as u32 {
                    return Err(StateError::internal(format!(
                        "Tree invalid format: {}",
                        tree.format
                    )));
                } else if tree.block_count > MAX_TREE_BLOCK_COUNT {
                    return Err(StateError::from(Oversized {
                        context: format!(
                            "tree block count {} exceeds limit {}",
                            tree.block_count, MAX_TREE_BLOCK_COUNT
                        ),
                    }));
                }
                {
                    let mut lock = self.runtime.write();
                    if let Some(current_tree) = &lock.tree {
                        tree = *current_tree;
                    } else {
                        lock.tree = Some(tree);
                    }
                }
                tree
            }
        };

        Ok(tree)
    }

    pub async fn cache_fragments(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<(), StateError> {
        let tree = self.tree(repository.clone()).await?;

        let mut address = Vec::with_capacity(5);

        address.push(Address::zero_context_hash(tree.hash_node));

        {
            let data = self.data.read();
            address.push(Address::zero_context_hash(data.hash_metadata));
            address.push(Address::zero_context_hash(data.hash_link));
        }

        address.push(Address::zero_context_hash(tree.hash_delta));
        address.push(Address::zero_context_hash(tree.hash_file_metadata));

        // Disregard any errors during caching
        let _ = immutable::cache(repository.clone(), address, true).await;

        /* Avoid caching all the blocks, generally it's better to fetch these on demand
           as it parallelizes better with other i/o

        // Cache the node blocks
        let buffer = immutable::read(
            repository.clone(),
            Address::zero_context_hash(tree.hash_node),
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(&repository).with_cache(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;

        let block_hash = buffer.as_type_slice::<Hash>();
        let block_address_count = block_hash.len();

        let address: Vec<Address> = block_hash[..block_address_count]
            .iter()
            .map(|&hash| Address::zero_context_hash(hash))
            .collect();

        let _ = immutable::cache(repository.clone(), address, false).await;
        */

        Ok(())
    }

    pub async fn revision_metadata(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<RevisionMetadata, StateError> {
        let metadata = Metadata::deserialize(repository, self.metadata_hash())
            .await
            .internal("Failed to deserialize metadata")?;

        Ok(RevisionMetadata::from_metadata(metadata))
    }

    pub fn link_merge_hash(&self) -> Hash {
        self.data.read().hash_link_merge
    }

    pub fn set_link_merge_hash(&self, hash: Hash) {
        let mut data = self.data.write();
        data.hash_link_merge = hash;
        data.flags |= StateFlags::Dirty;
    }

    pub fn clear_link_merge_state(&self) {
        let mut data = self.data.write();
        if !data.hash_link_merge.is_zero() {
            data.hash_link_merge = Hash::default();
            data.flags |= StateFlags::Dirty;
        }
    }

    pub async fn serialize_link_merge_state(
        &self,
        repository: Arc<RepositoryContext>,
        entries: &[LinkMergeEntry],
    ) -> Result<Hash, StateError> {
        let header = LinkMergeState {
            count: entries.len() as u32,
            flags: 0,
        };

        let mut bytes =
            Vec::with_capacity(size_of::<LinkMergeState>() + std::mem::size_of_val(entries));
        bytes.extend_from_slice(header.as_bytes());
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
        }

        let (address, _fragment) = immutable::write(
            repository.clone(),
            Context::default(),
            Bytes::from(bytes),
            immutable::write_options_from_repository(repository)
                .with_local_cache_priority()
                .with_max_size_chunk(),
        )
        .await
        .internal("Failed to serialize link merge state")?;

        self.set_link_merge_hash(address.hash);
        Ok(address.hash)
    }

    pub async fn deserialize_link_merge_state(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Vec<LinkMergeEntry>, StateError> {
        let hash = self.link_merge_hash();
        if hash.is_zero() {
            return Ok(vec![]);
        }

        let options = read_options_from_repository(&repository);
        let data = immutable::read(
            repository.clone(),
            Address::zero_context_hash(hash),
            None,
            options,
        )
        .await
        .internal("Failed to read link merge state")?;

        let raw = data.as_ref();
        let header_size = std::mem::size_of::<LinkMergeState>();
        if raw.len() < header_size {
            return Ok(vec![]);
        }

        let Ok(header) = LinkMergeState::read_from_bytes(&raw[..header_size]) else {
            return Ok(vec![]);
        };

        let mut entries = Vec::with_capacity(header.count as usize);
        let entry_bytes = &raw[header_size..];
        for chunk in entry_bytes
            .chunks_exact(size_of::<LinkMergeEntry>())
            .take(header.count as usize)
        {
            let Ok(entry) = LinkMergeEntry::read_from_bytes(chunk) else {
                break;
            };
            entries.push(entry);
        }

        Ok(entries)
    }

    pub async fn link_list(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Vec<LinkReference>, StateError> {
        let list_hash = { self.data.read().hash_link };

        let link_list = if !list_hash.is_zero() {
            let data = immutable::read(
                repository.clone(),
                Address::zero_context_hash(list_hash),
                None,
                immutable::read_options_from_repository(&repository)
                    .with_cache()
                    .with_priority(),
            )
            .await
            .internal("Failed to read state data")?
            .to_aligned::<LinkReference>();

            data.as_type_slice::<LinkReference>().to_vec()
        } else {
            vec![]
        };

        let mut runtime = self.runtime.write();
        let link_list = runtime.link_list.take().unwrap_or(link_list);

        Ok(link_list)
    }

    pub async fn link_find(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        local_node: NodeID,
    ) -> Result<LinkReference, StateError> {
        let link_list = {
            let runtime = self.runtime.read();
            runtime.link_list.clone()
        };

        let link_list = if let Some(link_list) = link_list {
            link_list
        } else {
            let list_hash = { self.data.read().hash_link };
            if !list_hash.is_zero() {
                let data = immutable::read(
                    repository.clone(),
                    Address::zero_context_hash(list_hash),
                    None,
                    immutable::read_options_from_repository(&repository)
                        .with_cache()
                        .with_priority(),
                )
                .await
                .internal("Failed to read state data")?
                .to_aligned::<LinkReference>();
                data.as_type_slice::<LinkReference>().to_vec()
            } else {
                vec![]
            }
        };

        for link in link_list.iter() {
            if link.repository == link_id && link.local_node == local_node {
                return Ok(*link);
            }
        }

        Err(LinkNotFound.into())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn link_add(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        branch: BranchId,
        signature: Hash,
        local_node: NodeID,
        link_flags: LinkFlags,
    ) -> Result<(), StateError> {
        let mut link_list = self.link_list(repository.clone()).await?;

        let mut runtime = self.runtime.write();

        // Ensure link is not referenced by other revision anywhere
        for link in link_list.iter_mut() {
            if link.repository == link_id && link.signature != signature {
                // TODO(vri): Link revision divergence
                return Err(StateError::internal("Link divergence"));
            }
            if link.repository == link_id && link.local_node == local_node {
                link.signature = signature;
                return Ok(());
            }
        }

        link_list.push(LinkReference {
            repository: link_id,
            branch,
            signature,
            local_node,
            flags: link_flags.into(),
            ..Default::default()
        });
        runtime.link_list = Some(link_list);

        Ok(())
    }

    pub async fn link_update(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        branch: BranchId,
        signature: Hash,
        local_node: NodeID,
    ) -> Result<(), StateError> {
        lore_debug!(
            "Update link with ID {link_id}, local node {local_node}, new signature {signature}, new branch {branch}"
        );

        let mut link_list = self.link_list(repository.clone()).await?;

        let mut runtime = self.runtime.write();

        for link in link_list.iter_mut() {
            if link.repository == link_id && link.local_node == local_node {
                link.branch = branch;
                link.signature = signature;
                runtime.link_list = Some(link_list);
                return Ok(());
            }
        }

        Err(LinkNotFound.into())
    }

    pub async fn link_remove(
        &self,
        repository: Arc<RepositoryContext>,
        link_id: RepositoryId,
        local_node: NodeID,
    ) -> Result<(), StateError> {
        lore_debug!("Remove link with ID {link_id}, local node {local_node}");
        let mut link_list = self.link_list(repository.clone()).await?;

        let mut runtime = self.runtime.write();

        if let Some(index) = link_list
            .iter()
            .position(|link| link.repository == link_id && link.local_node == local_node)
        {
            link_list.remove(index);
            runtime.link_list = Some(link_list.clone());
            return Ok(());
        }

        Err(LinkNotFound.into())
    }

    pub fn force_rehash_names(&self) {
        self.runtime.write().rehash_node_names = true;
    }

    pub async fn nametable(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<Arc<NameTable>, StateError> {
        {
            let runtime = self.runtime.read();
            if let Some(name_table) = runtime.name_table_deprecated.as_ref() {
                return Ok(name_table.clone());
            }
        }

        let _permit = self
            .deserialize
            .acquire()
            .await
            .internal("Failed to deserialize name table")?;

        let tree = self.tree(repository.clone()).await?;

        let name_table = {
            Arc::new(if !tree.hash_nametable_deprecated.is_zero() {
                NameTable::deserialize(repository, tree.hash_nametable_deprecated)
                    .await
                    .internal("Failed to deserialize name table")?
            } else {
                NameTable::default()
            })
        };

        {
            let mut runtime = self.runtime.write();
            if let Some(prev_name_table) = runtime.name_table_deprecated.as_ref() {
                return Ok(prev_name_table.clone());
            }

            runtime.name_table_deprecated = Some(name_table.clone());
        }

        Ok(name_table)
    }
}

/// Rebase the staged anchor onto a new current revision.
///
/// Callers (sync, branch switch, and similar operations that advance the
/// current revision pointer) invoke this after `store_current_anchor` to
/// keep the staged anchor consistent with the new current. The staged
/// anchor's contract is "current plus uncommitted modifications"; once
/// current moves, the anchor must either point at the new current (no
/// uncommitted work) or carry forward the uncommitted dirty paths.
///
/// Behavior:
/// - No staged anchor on disk: nothing to do.
/// - Anchor already equals `new_current_signature`: nothing to do.
/// - Anchor's tree has no dirty descendants: drop the anchor so the next
///   load falls back to the new current.
/// - Anchor's tree has dirty descendants: drop the anchor, then re-apply
///   each dirty path against the new current via [`crate::file::dirty::dirty`].
///   Only dirty nodes carry over; the prior staged merkle tree is discarded.
pub async fn rebase_staged_anchor(
    repository: Arc<RepositoryContext>,
    new_current_signature: Hash,
) -> Result<(), StateError> {
    let Some(old_staged_signature) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
    else {
        return Ok(());
    };

    if old_staged_signature == new_current_signature {
        return Ok(());
    }

    let old_staged_state = State::deserialize(repository.clone(), old_staged_signature).await?;
    let has_dirty = old_staged_state
        .node_has_dirty_children(repository.clone(), crate::node::ROOT_NODE)
        .await?;

    let _ = crate::instance::delete_staged_anchor(&repository).await;

    if !has_dirty {
        return Ok(());
    }

    let mut dirty_paths: Vec<RelativePath> = Vec::new();
    collect_dirty_paths(
        old_staged_state,
        repository.clone(),
        crate::node::ROOT_NODE,
        RelativePath::new(),
        &mut dirty_paths,
    )
    .await?;

    if dirty_paths.is_empty() {
        return Ok(());
    }

    crate::file::dirty::dirty_relative_paths(repository, dirty_paths)
        .await
        .forward::<StateError>("Failed to apply dirty paths during staged rebase")?;

    Ok(())
}

/// Walk a staged state and collect paths of nodes carrying an explicit dirty
/// action (`DirtyAdd`/`DirtyModify`/`DirtyDelete`/`DirtyMove`/`DirtyCopy`).
///
/// Propagated-only `Dirty` parents are walked but not recorded — only the
/// leaves with concrete actions need re-application. Descent stops at link
/// boundaries (children live in another repository's state) and at
/// `DirtyDelete`/`DirtyMove` subtrees (the parent action carries the whole
/// subtree when re-applied).
pub(crate) fn collect_dirty_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    parent_path: RelativePath,
    paths: &mut Vec<RelativePath>,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + '_>> {
    collect_dirty_paths_inner(state, repository, parent_node, parent_path, paths, false)
}

/// Like [`collect_dirty_paths`] but skips nodes that are also staged.
///
/// Used by the commit pipeline to capture only paths that should be
/// re-applied as a new staged anchor on top of the freshly committed
/// revision — staged paths are already part of the new commit and would be
/// incorrectly re-marked as `DirtyModify` by `file::dirty::dirty()` if
/// included.
pub(crate) fn collect_dirty_only_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    parent_path: RelativePath,
    paths: &mut Vec<RelativePath>,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + '_>> {
    collect_dirty_paths_inner(state, repository, parent_node, parent_path, paths, true)
}

fn collect_dirty_paths_inner(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    parent_path: RelativePath,
    paths: &mut Vec<RelativePath>,
    skip_staged: bool,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + '_>> {
    Box::pin(async move {
        let node = state.node(repository.clone(), parent_node).await?;
        if node.is_link() || !node.is_directory() {
            return Ok(());
        }

        let mut child_node_opt = node.child();
        let mut cycle = SiblingCycleGuard::new(parent_node);
        while let Some(child_id) = child_node_opt {
            let child = state.node(repository.clone(), child_id).await?;
            child.walk_step(child_id, parent_node, &mut cycle)?;

            if !child.is_dirty() {
                child_node_opt = child.sibling();
                continue;
            }

            let child_name = state.node_name_clone(repository.clone(), child_id).await?;
            let child_path = parent_path.push_into_buf(&child_name).freeze();

            // Don't carry forward dirty paths that the view/ignore filter
            // excludes — they cannot be re-applied against a checkout that
            // never materializes them. --force bypasses the filter.
            let force = execution_context().globals().force();
            if !force
                && repository
                    .filter
                    .excludes(&child_path, child.is_directory(), FilterMode::Full)
            {
                child_node_opt = child.sibling();
                continue;
            }

            let action_bits = NodeFlags::from_bits_truncate(child.flags) & NodeFlags::ActionBits;
            let skip_for_staged = skip_staged && child.is_staged();
            if !action_bits.is_empty() && !skip_for_staged {
                paths.push(child_path.clone());
            }

            let stop_subtree = child.is_dirty_delete() || child.is_dirty_move();
            if child.is_directory() && !stop_subtree {
                collect_dirty_paths_inner(
                    state.clone(),
                    repository.clone(),
                    child_id,
                    child_path,
                    paths,
                    skip_staged,
                )
                .await?;
            }

            child_node_opt = child.sibling();
        }

        Ok(())
    })
}

pub struct TreePath {
    pub path: RelativePath,
    pub address: Option<Address>,
    pub flags: NodeFlags,
}

pub type CanReadRepository = Arc<dyn Fn(RepositoryId) -> bool + Send + Sync>;

pub fn allow_all_repositories() -> CanReadRepository {
    Arc::new(|_| true)
}

/// Maximum number of link hops a tree walk follows before giving up. Bounds
/// both unbounded link chains and link cycles so a walk terminates.
pub const MAX_LINK_DEPTH: usize = 8;

pub async fn gather_tree_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    path: RelativePath,
    max_depth: usize,
    can_read: CanReadRepository,
) -> Result<Vec<TreePath>, StateError> {
    let (walk_state, walk_repository, parent_node_id) = if path.is_empty() {
        (state, repository, ROOT_NODE)
    } else {
        let node_link = state
            .find_node_link(repository.clone(), path.as_str())
            .await?;
        if !node_link.is_valid() {
            return Err(NodeNotFound.into());
        }
        if !can_read(node_link.repository) {
            lore_debug!(
                "Path resolution stops at unauthorized repository {}",
                node_link.repository,
            );
            return Err(NodeNotFound.into());
        }
        if node_link.revision == state.revision() {
            (state, repository, node_link.node)
        } else {
            let linked_repo = Arc::new(repository.to_link_context(node_link.repository).await);
            let linked_state = State::deserialize(linked_repo.clone(), node_link.revision).await?;
            (linked_state, linked_repo, node_link.node)
        }
    };

    let mut paths: Vec<TreePath> = Vec::new();
    enumerate_children(
        walk_state,
        walk_repository,
        parent_node_id,
        path,
        0,
        max_depth,
        0,
        can_read,
        &mut paths,
    )
    .await?;
    Ok(paths)
}

#[allow(clippy::too_many_arguments)]
async fn enumerate_children(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node_id: NodeID,
    parent_path: RelativePath,
    depth: usize,
    max_depth: usize,
    link_depth: usize,
    can_read: CanReadRepository,
    result: &mut Vec<TreePath>,
) -> Result<(), StateError> {
    let block_index = NodeBlock::index(parent_node_id);
    let node_index = Node::index(parent_node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let parent = block.node(node_index);
    if !parent.is_directory() {
        return Err(InvalidPath {
            path: parent_path.to_string(),
        }
        .into());
    }
    let mut cycle = SiblingCycleGuard::new(parent_node_id);
    gather_tree_paths_node_recurse(
        state,
        repository,
        parent.child(),
        parent_node_id,
        parent_path,
        depth,
        max_depth,
        link_depth,
        can_read,
        result,
        &mut cycle,
    )
    .await
}

fn log_linked_subtree_failure(
    prefix: &str,
    repository: RepositoryId,
    revision: Hash,
    err: &StateError,
) {
    match err {
        StateErrors::NotFound(_)
        | StateErrors::NodeNotFound(_)
        | StateErrors::LinkNotFound(_)
        | StateErrors::RevisionNotFound(_)
        | StateErrors::AddressNotFound(_) => {
            lore_debug!("{prefix} at {repository} @ {revision}: {err}");
        }
        _ => {
            lore_warn!("{prefix} at {repository} @ {revision}: {err}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn gather_tree_paths_node(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    expected_parent: NodeID,
    parent_path: RelativePath,
    depth: usize,
    max_depth: usize,
    link_depth: usize,
    can_read: CanReadRepository,
    result: &mut Vec<TreePath>,
    cycle: &mut SiblingCycleGuard,
) -> Result<Option<NodeID>, StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await?;
    let node = block.node(node_index);
    node.walk_step(node_id, expected_parent, cycle)?;

    let node_name = block.node_name_ref(node_index).internal("Node name")?;
    let node_path = if parent_path.is_empty() {
        RelativePath::new_from_initial_path(node_name).unwrap_or_default()
    } else {
        parent_path.push_into_buf(node_name).freeze()
    };
    let address = if node.is_directory() {
        None
    } else {
        Some(node.address)
    };
    let flags = if node.is_file() {
        NodeFlags::File
    } else if node.is_link() {
        NodeFlags::Link
    } else {
        NodeFlags::NoFlags
    };
    result.push(TreePath {
        path: node_path.clone(),
        address,
        flags,
    });

    let depth_remaining = max_depth == 0 || depth + 1 < max_depth;
    if node.is_directory() && depth_remaining {
        let mut child_cycle = SiblingCycleGuard::new(node_id);
        gather_tree_paths_node_recurse(
            state.clone(),
            repository.clone(),
            node.child(),
            node_id,
            node_path,
            depth + 1,
            max_depth,
            link_depth,
            can_read,
            result,
            &mut child_cycle,
        )
        .await?;
    } else if node.is_link() && depth_remaining && link_depth < MAX_LINK_DEPTH {
        let link = node.linked_node();
        if !can_read(link.repository) {
            lore_debug!(
                "Skipping linked subtree: caller not authorized for repository {}",
                link.repository,
            );
        } else {
            let linked_repo = Arc::new(repository.to_link_context(link.repository).await);
            match State::deserialize(linked_repo.clone(), link.revision).await {
                Ok(linked_state) => {
                    if let Err(err) = enumerate_children(
                        linked_state,
                        linked_repo,
                        link.node,
                        node_path,
                        depth + 1,
                        max_depth,
                        link_depth + 1,
                        can_read,
                        result,
                    )
                    .await
                    {
                        log_linked_subtree_failure(
                            "Aborting linked subtree",
                            link.repository,
                            link.revision,
                            &err,
                        );
                    }
                }
                Err(err) => {
                    log_linked_subtree_failure(
                        "Skipping linked subtree: cannot load state",
                        link.repository,
                        link.revision,
                        &err,
                    );
                }
            }
        }
    }

    Ok(node.sibling())
}

#[allow(clippy::too_many_arguments)]
fn gather_tree_paths_node_recurse<'a>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    first_child: Option<NodeID>,
    expected_parent: NodeID,
    parent_path: RelativePath,
    depth: usize,
    max_depth: usize,
    link_depth: usize,
    can_read: CanReadRepository,
    result: &'a mut Vec<TreePath>,
    cycle: &'a mut SiblingCycleGuard,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
    Box::pin(async move {
        let mut next = first_child;
        while let Some(node_id) = next {
            next = gather_tree_paths_node(
                state.clone(),
                repository.clone(),
                node_id,
                expected_parent,
                parent_path.clone(),
                depth,
                max_depth,
                link_depth,
                can_read.clone(),
                result,
                cycle,
            )
            .await?;
        }
        Ok(())
    })
}

/// Discard a single node and patch the parent/sibling hierarchy links
/// to remove the node from the linked list. This has to be done in serial
/// as a post-processing step of the commit operation to avoid different
/// tasks modifying the parent/sibling pointers of related nodes during
/// the hierarchy walk to find related nodes.
pub async fn node_discard_patch<F>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    handler: F,
) -> Result<usize, StateError>
where
    F: Fn(NodeID, u16) + Clone + Send + 'static,
{
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let node = block.node(node_index);

    // Remap any previous child/sibling node to point to the new "next" node
    lore_trace!("Remapping child/sibling links for node {node_id}",);

    let mut found_node = false;
    let parent_block_index = NodeBlock::index(node.parent);
    let parent_node_index = Node::index(node.parent);
    let mut prev_sibling_id = {
        let parent_block = state.block(repository.clone(), parent_block_index).await?;
        let parent_node = parent_block.node(parent_node_index);
        if parent_node.child == node_id {
            lore_trace!(
                "Child link on parent node {} matching node {}",
                node.parent,
                node_id
            );
            // Since patched deletion of nodes is done in a serial fashion we
            // don't need a read lock on current block to ensure sibling is
            // still accurate - we can use the previously fetched node info
            {
                let mut parent_block_writer = parent_block.write();
                parent_block_writer.node(parent_node_index).child = node.sibling;
                if parent_block_writer.mark_dirty() {
                    state.block_modified(parent_block.clone(), parent_block_index);
                }
            }
            lore_trace!(
                "Child link on parent node {} remapped from node {} -> node {} (expected {})",
                node.parent,
                node_id,
                parent_block.node(parent_node_index).child,
                node.sibling
            );
            found_node = true;
            INVALID_NODE
        } else {
            lore_trace!(
                "Child link on parent node {} is node {}, not node {} - walk list",
                node.parent,
                parent_node.child,
                node_id
            );
            parent_node.child
        }
    };
    while prev_sibling_id.is_valid_node_id() {
        let sibling_block_index = NodeBlock::index(prev_sibling_id);
        let sibling_node_index = Node::index(prev_sibling_id);
        let sibling_block = state.block(repository.clone(), sibling_block_index).await?;
        let sibling_node = sibling_block.node(sibling_node_index);
        if sibling_node.sibling == node_id {
            lore_trace!(
                "Sibling link on node {} matching node {}",
                prev_sibling_id,
                node_id
            );
            // Since patched deletion of nodes is done in a serial fashion we
            // don't need a read lock on current block to ensure sibling is
            // still accurate - we can use the previously fetched node info
            {
                let mut sibling_block_writer = sibling_block.write();
                sibling_block_writer.node(sibling_node_index).sibling = node.sibling;
                if sibling_block_writer.mark_dirty() {
                    state.block_modified(sibling_block.clone(), sibling_block_index);
                }
            }
            lore_trace!(
                "Sibling link on node {} remapped from node {} -> node {} (expected {})",
                prev_sibling_id,
                node_id,
                sibling_block.node(sibling_node_index).sibling,
                node.sibling
            );
            found_node = true;
            break;
        } else {
            lore_trace!(
                "Sibling link on node {} is node {}, not node {} - continue walk list",
                prev_sibling_id,
                sibling_node.sibling,
                node_id
            );
            prev_sibling_id = sibling_node.sibling;
        }
    }

    if !found_node {
        let chain = format_parent_child_chain(&state, &repository, node.parent).await;
        return Err(StateError::internal(format!(
            "Discard hierarchy broken: node {node_id} (parent={parent_node_id}, \
             sibling_in_node={node_sibling}, flags={node_flags:#x}) not in \
             parent.child chain (observed: {chain})",
            parent_node_id = node.parent,
            node_sibling = node.sibling,
            node_flags = node.flags,
        )));
    }

    handler(node_id, node.flags);

    let dirtied = {
        lore_trace!("Updating block to discard node {}", node_id);
        let mut lock = block.write();
        lock.discard_node(block_index, node_index);
        lock.mark_dirty()
    };
    if dirtied {
        state.block_modified(block.clone(), block_index);
        state.mark_dirty();
    }

    Ok(1)
}

/// Read-only walk of `parent_node_id`'s `child → sibling → …` chain
/// formatted for diagnostic error messages. Called only from the error
/// path of [`node_discard_patch`] so it never costs on the hot path.
async fn format_parent_child_chain(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    parent_node_id: NodeID,
) -> String {
    const MAX_CHAIN: usize = 64;

    let Ok(parent_block) = state
        .block(repository.clone(), NodeBlock::index(parent_node_id))
        .await
    else {
        return "<parent block unreadable>".to_string();
    };
    let initial_child = parent_block.node(Node::index(parent_node_id)).child;

    let mut buffer = String::new();
    if !initial_child.is_valid_node_id() {
        buffer.push_str("<empty>");
        return buffer;
    }

    let mut current_node_id = initial_child;
    let mut steps = 0;
    while current_node_id.is_valid_node_id() {
        if steps == MAX_CHAIN {
            buffer.push_str(" -> …(truncated)");
            return buffer;
        }
        if steps > 0 {
            buffer.push_str(" -> ");
        }
        buffer.push_str(&current_node_id.to_string());
        steps += 1;

        let Ok(sibling_block) = state
            .block(repository.clone(), NodeBlock::index(current_node_id))
            .await
        else {
            buffer.push_str(" -> <unreadable>");
            return buffer;
        };
        current_node_id = sibling_block.node(Node::index(current_node_id)).sibling;
    }
    buffer
}

/// Counts of files and directories discarded during a node discard operation.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscardCounts {
    pub file_count: u64,
    pub directory_count: u64,
}

/// Discard a node and all child nodes in the subhierarchy in case this is a directory node.
/// Will not patch any parent/sibling pointers and should only be called on the child nodes
/// of the initial node being discarded in a commit operation. This allows the subhierarchy
/// node discard to happen in parallel as is does not modify hierarchy parent/sibling linked
/// lists, while deferring the discard on the initial node which needs hierarchy patching to
/// a serial post-process step.
pub async fn node_discard_nopatch<F>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    recurse: bool,
    discard: bool,
    handler: F,
) -> Result<DiscardCounts, StateError>
where
    F: Fn(NodeID, u16) + Clone + Send + 'static,
{
    let mut counts = DiscardCounts::default();
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let node = block.node(node_index);

    if recurse && node.is_directory() {
        // Directory, discard all children recursively, but no need to patch up parent/child/sibling pointers
        // as all the nodes are discarded anyway
        lore_trace!("Recursively discarding directory node {node_id}",);
        let mut tasks = JoinSet::new();
        let mut child_node_ref = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_ref {
            let child_block_index = NodeBlock::index(child_node_id);
            let child_node_index = Node::index(child_node_id);

            let child_block = state.block(repository.clone(), child_block_index).await?;
            let child_node = child_block.node(child_node_index);

            child_node.walk_step(child_node_id, node_id, &mut cycle)?;

            lore_spawn!(tasks, {
                let state = state.clone();
                let repository = repository.clone();
                let handler = handler.clone();
                async move {
                    node_discard_recurse(
                        state,
                        repository,
                        child_node_id,
                        recurse,
                        discard,
                        handler,
                    )
                    .await
                }
            });

            child_node_ref = child_node.sibling();
        }

        let mut task_failure = Ok(());
        while let Some(task) = tasks.join_next().await {
            if let Ok(result) = task {
                let child_counts = result?;
                counts.file_count += child_counts.file_count;
                counts.directory_count += child_counts.directory_count;
            } else {
                task_failure = Err(task.unwrap_err());
            }
        }
        task_failure.internal("Discard node task")?;
    }

    handler(node_id, node.flags);

    if discard {
        let dirtied = {
            lore_trace!("Updating block to discard node {}", node_id);
            let mut lock = block.write();
            lock.discard_node(block_index, node_index);
            lock.mark_dirty()
        };
        if dirtied {
            state.block_modified(block.clone(), block_index);
            state.mark_dirty();
        }
    }

    if node.is_directory() {
        counts.directory_count += 1;
    } else {
        counts.file_count += 1;
    }
    Ok(counts)
}

fn node_discard_recurse<F>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    recurse: bool,
    discard: bool,
    handler: F,
) -> Pin<Box<dyn Future<Output = Result<DiscardCounts, StateError>> + Send>>
where
    F: Fn(NodeID, u16) + Clone + Send + 'static,
{
    Box::pin(node_discard_nopatch(
        state, repository, node_id, recurse, discard, handler,
    ))
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TreeFlags: u32 {
        /// Tree is dirty
        const Dirty = 0b1;
    }
}
bitflagsops!(TreeFlags, u32);

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, FromBytes, Immutable)]
pub struct Tree {
    /// Magic identifier
    pub magic: u32,
    /// Format version
    pub format: u32,
    /// Tree flags
    pub flags: u32,
    /// Node and file metadata block count
    pub block_count: u32,
    /// Delta count
    pub delta_count: u32,
    /// First block with unused node slots
    block_unused_first: u32,
    /// Size of the full tree in bytes
    pub size: u64,
    /// Root hash
    pub hash_root: Hash,
    /// Node blocks fragment
    pub hash_node: Hash,
    /// Nametable fragment
    pub hash_nametable_deprecated: Hash,
    /// File metadata blocks fragment
    pub hash_file_metadata: Hash,
    /// Delta fragment
    pub hash_delta: Hash,
    /// Reserved for future extension
    hash_reserved: [Hash; 3],
}

impl Default for Tree {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

const TREE_MAGIC: u32 = 0x3C71BF05u32;

/// Maximum number of blocks in a tree. Guards against malicious or corrupt
/// tree headers triggering unbounded allocations when blocks are accessed.
pub const MAX_TREE_BLOCK_COUNT: u32 = 1_000_000;

/// Tree format version identifiers
#[repr(u32)]
pub enum TreeFormat {
    /// Initial version
    Initial = 1,
}

fn named_node_sort(node: &mut [StateNamedNode]) {
    node.sort_unstable_by_key(|lhs| lhs.name);
}

/// Compute change flags from node state and action context.
/// This is a pure function that extracts flag computation logic.
pub fn compute_change_flags(node: &Node, action: FileAction, to_node_valid: bool) -> change::Flags {
    let mut flags = change::Flags::None;

    // If this change represents revision -> filesystem change, set modified flag for keep action
    if !to_node_valid && action == FileAction::Keep {
        flags |= change::Flags::Modify;
    }

    if node.is_staged() {
        flags |= change::Flags::Staged;
    }
    if node.is_dirty() {
        flags |= change::Flags::Dirty;
    }
    if node.is_staged_merge() {
        flags |= change::Flags::Merge;
    }
    if node.is_staged_merge_conflict() {
        flags |= change::Flags::Conflict;
    }
    if node.is_staged_merge_resolved() {
        flags |= change::Flags::ConflictResolved;
    }
    if node.is_staged_merge_mine() {
        flags |= change::Flags::ConflictMine;
    }
    if node.is_staged_merge_theirs() {
        flags |= change::Flags::ConflictTheirs;
    }

    flags
}

/// Indicates which node source to use for loading node data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSource {
    /// Use the 'from' node (typically for Delete actions)
    From,
    /// Use the 'to' node (typically for Add/Keep actions)
    To,
    /// No valid node available (filesystem-only paths)
    Invalid,
}

/// Determine which node source to use based on action and node validity.
pub fn determine_node_source(_action: FileAction, from_valid: bool, to_valid: bool) -> NodeSource {
    if !to_valid {
        if from_valid {
            NodeSource::From
        } else {
            NodeSource::Invalid
        }
    } else {
        NodeSource::To
    }
}

/// Load a node based on the determined source.
async fn load_node_for_change(
    source: NodeSource,
    from: &NodeChangeState,
    to: &NodeChangeState,
) -> Option<Node> {
    match source {
        NodeSource::From => {
            let block_index = NodeBlock::index(from.node);
            let node_index = Node::index(from.node);
            from.state
                .block(from.repository.clone(), block_index)
                .await
                .ok()
                .map(|block| block.node(node_index))
        }
        NodeSource::To => {
            let block_index = NodeBlock::index(to.node);
            let node_index = Node::index(to.node);
            to.state
                .block(to.repository.clone(), block_index)
                .await
                .ok()
                .map(|block| block.node(node_index))
        }
        NodeSource::Invalid => Some(Node::default()),
    }
}

async fn add_change(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    path: &RelativePath,
    from_path: Option<&RelativePath>,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    // Avoid adding repository root node in case it was to/from an empty repository
    if from.node != ROOT_NODE || to.node != ROOT_NODE {
        // Determine which node to use and load it
        let source = determine_node_source(
            action,
            from.node.is_valid_node_id(),
            to.node.is_valid_node_id(),
        );

        // Only add (file system path not in merkle tree) should end up here for Invalid source
        debug_assert!(source != NodeSource::Invalid || action == FileAction::Add);

        let Some(node) = load_node_for_change(source, &from, &to).await else {
            return Ok(());
        };

        // Compute flags and create change record
        let flags = compute_change_flags(&node, action, to.node.is_valid_node_id());

        sink.emit(NodeChange {
            action,
            flags,
            from: from.clone(),
            to: to.clone(),
            path: path.clone(),
            from_path: from_path.cloned(),
        })
        .await?;

        // Recursion happens in caller for directories and links
        if node.is_file() {
            return Ok(());
        }
    }

    if action == change::FileAction::Keep {
        // Recursion happens in caller for modifications and stages
        return Ok(());
    }

    Box::pin(async move { add_change_hierarchy(from, to, action, path, sink, filter_mode).await })
        .await
}

/// Dispatch hierarchy traversal to the appropriate handler based on action.
async fn add_change_hierarchy(
    from: NodeChangeState,
    to: NodeChangeState,
    action: change::FileAction,
    path: &RelativePath,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    match action {
        FileAction::Delete => add_hierarchy_delete(from, to, path, sink, filter_mode).await?,
        FileAction::Add => add_hierarchy_add(from, to, path, sink, filter_mode).await?,
        _ => {} // Keep/Copy/Move don't recurse here
    }
    Ok(())
}

/// Recursively add delete changes for an entire directory hierarchy.
async fn add_hierarchy_delete(
    from: NodeChangeState,
    to: NodeChangeState,
    path: &RelativePath,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    // Try to get nodes from both states first
    let from_node = if from.node.is_valid_or_root_node_id() {
        from.state
            .node(from.repository.clone(), from.node)
            .await
            .ok()
    } else {
        None
    };

    let to_node = if to.node.is_valid_or_root_node_id() {
        to.state.node(to.repository.clone(), to.node).await.ok()
    } else {
        None
    };

    // Choose the state, "from" for normal deletions, "to" for merge deletions
    let (iteration_state, node) = if let Some(from_node) = from_node {
        (from, Some(from_node))
    } else if let Some(to_node) = to_node {
        (to.clone(), Some(to_node))
    } else {
        return Ok(());
    };

    // File nodes end recursion
    if node.map(|n| n.is_file()).unwrap_or_default() {
        return Ok(());
    }

    // Link nodes don't recurse - don't show individual link files as deleted
    if node.map(|n| n.is_link()).unwrap_or_default() {
        return Ok(());
    }

    // Iterate children from whichever state has the node
    let mut children = StateNodeChildrenWithNameIterator::new(
        iteration_state.state.clone(),
        iteration_state.repository.clone(),
        iteration_state.node,
    )
    .await?;

    while let Some((child_id, child_node, child_name)) = children.next().await? {
        let child_path = path.push_into_buf(child_name).freeze();

        // Skip excluded paths
        if iteration_state.repository.filter.emit_excludes(
            &child_path,
            child_node.is_directory(),
            filter_mode,
        ) {
            continue;
        }

        let child_from = iteration_state.from_child(child_id, &child_node);

        Box::pin(add_change(
            child_from,
            to.invalid(),
            FileAction::Delete,
            &child_path,
            None,
            sink,
            filter_mode,
        ))
        .await?;
    }
    Ok(())
}

/// Recursively add add changes for an entire directory hierarchy.
async fn add_hierarchy_add(
    from: NodeChangeState,
    to: NodeChangeState,
    path: &RelativePath,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    // Check early exit conditions
    let to_node = if to.node.is_valid_or_root_node_id() {
        to.state.node(to.repository.clone(), to.node).await.ok()
    } else {
        None
    };

    // File nodes end recursion
    if to_node.map(|n| n.is_file()).unwrap_or_default() {
        return Ok(());
    }

    // Link nodes don't recurse
    // TODO(UCS-11623): Check if the target link repository has no local changes - if so, do not
    // iterate and show each link file as added. Otherwise, recurse in and compare against file
    // system and/or staged state in link
    if to_node.map(|n| n.is_link()).unwrap_or_default() {
        return Ok(());
    }

    let mut children =
        StateNodeChildrenWithNameIterator::new(to.state.clone(), to.repository.clone(), to.node)
            .await?;

    while let Some((child_id, child_node, child_name)) = children.next().await? {
        let child_path = path.push_into_buf(child_name).freeze();

        // Skip excluded paths
        if to
            .repository
            .filter
            .emit_excludes(&child_path, child_node.is_directory(), filter_mode)
        {
            continue;
        }

        let child_to = to.from_child(child_id, &child_node);
        Box::pin(add_change(
            from.invalid(),
            child_to,
            FileAction::Add,
            &child_path,
            None,
            sink,
            filter_mode,
        ))
        .await?;
    }
    Ok(())
}

/// Detect and coalesce add/delete pairs that represent file moves.
///
/// Files are identified by their context (file ID) in the node address.
/// When an add and delete have the same non-zero context, they represent
/// a move operation and should be coalesced into a single move change.
///
/// This function modifies the changes vector in-place:
/// - Matching add/delete pairs are converted to move actions
/// - The delete change is marked for removal (action set to Keep with empty path)
/// - The add change is converted to a Move with `from_path` set
pub fn detect_and_coalesce_moves(changes: &mut Vec<NodeChange>) {
    let mut adds: Vec<(usize, Context)> = Vec::new();
    let mut deletes: Vec<(usize, Context)> = Vec::new();

    for index in 0..changes.len() {
        match changes[index].action {
            FileAction::Add => {
                let context = changes[index].to.address.context;
                if context.is_zero() {
                    continue;
                }

                let matching_delete_pos = deletes
                    .iter()
                    .position(|(_, delete_context)| *delete_context == context);

                if let Some(delete_vec_index) = matching_delete_pos {
                    // Found a match - coalesce into a move immediately
                    let (delete_index, _) = deletes.remove(delete_vec_index);

                    // Extract data from the delete change
                    let from_path = changes[delete_index].path.clone();
                    let from_state = changes[delete_index].from.clone();

                    lore_trace!(
                        "Detected move: {} -> {}",
                        from_path.as_str(),
                        changes[index].path.as_str()
                    );

                    changes[index].action = FileAction::Move;
                    changes[index].from_path = Some(from_path);
                    changes[index].from = from_state;

                    changes[delete_index].action = FileAction::Keep;
                    changes[delete_index].path = RelativePath::new();
                } else {
                    adds.push((index, context));
                }
            }
            FileAction::Delete => {
                let context = changes[index].from.address.context;
                if context.is_zero() {
                    continue;
                }

                let matching_add_pos = adds
                    .iter()
                    .position(|(_, add_context)| *add_context == context);

                if let Some(add_vec_index) = matching_add_pos {
                    let (add_index, _) = adds.remove(add_vec_index);

                    let from_path = changes[index].path.clone();
                    let from_state = changes[index].from.clone();

                    lore_trace!(
                        "Detected move: {} -> {}",
                        from_path.as_str(),
                        changes[add_index].path.as_str()
                    );

                    changes[add_index].action = FileAction::Move;
                    changes[add_index].from_path = Some(from_path);
                    changes[add_index].from = from_state;

                    changes[index].action = FileAction::Keep;
                    changes[index].path = RelativePath::new();
                } else {
                    deletes.push((index, context));
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < changes.len() {
        if changes[i].action == FileAction::Keep && changes[i].path.is_empty() {
            changes.swap_remove(i);
        } else {
            i += 1;
        }
    }
}

/// Calculate the set of changes between two revision states and emit them
/// into `sink`. Streams raw `Add` / `Delete` / `Keep` records as discovered
/// — does **not** run the post-walk move-coalescing or path-sort fixup that
/// the legacy `Vec`-returning version applied. Callers that want the
/// historical buffered-and-coalesced shape use `diff_collect` instead.
pub async fn diff(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_to: Arc<RepositoryContext>,
    state_to: Arc<State>,
    path: Option<RelativePath>,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    if let Some(path) = path {
        let from_link = state_from
            .find_node_link(repository_from.clone(), path.as_str())
            .await
            .unwrap_or(NodeLink::invalid());
        let to_link = state_to
            .find_node_link(repository_to.clone(), path.as_str())
            .await
            .unwrap_or(NodeLink::invalid());

        let mut repository_from = repository_from;
        let state_from = if !from_link.repository.is_zero()
            && from_link.repository != repository_from.id
        {
            repository_from = Arc::new(repository_from.to_link_context(from_link.repository).await);
            State::deserialize(repository_from.clone(), from_link.revision).await?
        } else {
            state_from
        };

        let mut repository_to = repository_to;
        let state_to = if !to_link.repository.is_zero() && to_link.repository != repository_to.id {
            repository_to = Arc::new(repository_to.to_link_context(to_link.repository).await);
            State::deserialize(repository_to.clone(), to_link.revision).await?
        } else {
            state_to
        };

        async fn make_node_change_state(
            repository: &Arc<RepositoryContext>,
            state: &Arc<State>,
            node_id: NodeID,
        ) -> NodeChangeState {
            let (address, flags) = if let Ok(node) = state.node(repository.clone(), node_id).await {
                (node.address, NodeFlags::from_bits_retain(node.flags))
            } else {
                (Address::default(), NodeFlags::NoFlags)
            };
            NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: node_id,
                flags,
                address,
            }
        }
        let from = make_node_change_state(&repository_from, &state_from, from_link.node).await;
        let to = make_node_change_state(&repository_to, &state_to, to_link.node).await;

        diff::diff_subtree(from, to, path, 0, sink, filter_mode).await?;
    } else {
        diff::diff_subtree(
            NodeChangeState {
                repository: repository_from,
                state: state_from,
                node: ROOT_NODE,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
            },
            NodeChangeState {
                repository: repository_to,
                state: state_to,
                node: ROOT_NODE,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
            },
            RelativePath::new(),
            0,
            sink,
            filter_mode,
        )
        .await?;
    }

    Ok(())
}

/// Collect the set of changes between two revision states into a `Vec`,
/// preserving the post-walk move-coalescing and path-sort that the legacy
/// `state::diff` performed. Callers that want a `Vec<NodeChange>` use this
/// wrapper; callers that want streaming use `diff` directly.
pub async fn diff_collect(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_to: Arc<RepositoryContext>,
    state_to: Arc<State>,
    path: Option<RelativePath>,
    filter_mode: FilterMode,
) -> Result<Vec<NodeChange>, StateError> {
    let mut changes: Vec<NodeChange> = Vec::new();
    {
        let mut sink = ChangeSink::Vec(&mut changes);
        diff(
            repository_from,
            state_from,
            repository_to,
            state_to,
            path,
            &mut sink,
            filter_mode,
        )
        .await?;
    }
    detect_and_coalesce_moves(&mut changes);
    // Re-sort after move coalescing which uses swap_remove and can break path order.
    crate::change::sort_by_path(&mut changes);
    Ok(changes)
}

#[derive(Default)]
pub struct FilesystemDiffStats {
    pub file_add: AtomicU64,
    pub file_delete: AtomicU64,
    pub file_retain: AtomicU64,
    pub file_replace: AtomicU64,
}

impl FilesystemDiffStats {
    fn append(&mut self, stats: FilesystemDiffStats) {
        self.file_add
            .fetch_add(stats.file_add.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_delete
            .fetch_add(stats.file_delete.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_retain
            .fetch_add(stats.file_retain.load(Ordering::Relaxed), Ordering::Relaxed);
        self.file_replace.fetch_add(
            stats.file_replace.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}

/// Information about a configured layer mount used by `diff_filesystem` to
/// switch comparison context when the filesystem walk crosses into a layer.
///
/// When the parent's filesystem walker encounters a directory whose
/// parent-relative path equals `target_path`, it stops treating the directory
/// as a "new" entry and instead recurses into it using `repository` and
/// `state` — i.e., compares the on-disk content against the layer's tree
/// rather than the parent's.
#[derive(Clone, Debug)]
pub struct LayerMountInfo {
    /// Parent-relative mount path (e.g. `"external/lib"`).
    pub target_path: String,
    /// The layer's repository context — node lookups during the recursion
    /// resolve in this repo.
    pub repository: Arc<RepositoryContext>,
    /// State to diff against. Typically the layer's staged state (falling
    /// back to current when no staging has happened).
    pub state: Arc<State>,
    /// Node ID in `state` corresponding to the layer's `source_path`.
    pub source_node: NodeID,
}

/// Information about a link mount in the current state, gathered once at the
/// top of `diff_filesystem_ex` so the per-directory walk can detect "this
/// filesystem directory is a link, not a fresh add" with a single linear
/// `find` instead of an async block-walk per directory.
///
/// Only `target_path` is needed today because the link-mount handling skips
/// recursion entirely (the link is the parent-tree change; its content is
/// owned by the linked repository). If we later want the walker to recurse
/// into a linked state for some operation, extend this struct rather than
/// re-introducing the per-directory `find_node_link` lookup.
struct LinkMountInfo {
    /// Parent-relative mount path of the link node (e.g. `"libs/shared"`).
    target_path: String,
}

/// Enumerate every link in `state` and resolve its parent-relative mount
/// path. The result is shared by reference through `DiffFilesystemContext`
/// so the per-directory walk avoids an O(depth) block-walk per new directory
/// on fresh checkouts.
async fn collect_link_mounts(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
) -> Result<Vec<LinkMountInfo>, StateError> {
    let link_list = state.link_list(repository.clone()).await?;
    let mut mounts = Vec::with_capacity(link_list.len());
    for link_ref in link_list.iter() {
        let target_path = state
            .node_path(repository.clone(), link_ref.local_node as NodeID)
            .await?;
        mounts.push(LinkMountInfo { target_path });
    }
    Ok(mounts)
}

/// Calculate the set of changes from state to filesystem. Since the file system timestamp tracking
/// only tells if a file is unmodified compared to last write, we need the current state as well to
/// tell what that last write was.
///
/// `layer_mounts` is consulted only by the parent's filesystem walker to
/// switch context when crossing into a configured layer. Pass an empty Arc
/// for non-layer-aware callers; the layer-internal recursion always passes
/// empty (no nested layer mounts under non-overlapping layers).
pub async fn diff_filesystem(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    path: Option<RelativePath>,
    filter_mode: FilterMode,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    diff_filesystem_ex(
        repository_from,
        state_from,
        repository_current,
        state_current,
        path,
        filter_mode,
        false,
        layer_mounts,
    )
    .await
}

/// Extended version of `diff_filesystem` with `scan_dirty` support.
/// When `scan_dirty` is true, Dirty flags are set on modified files and cleared on
/// retained (unmodified) files inline during the walk.
#[allow(clippy::too_many_arguments)]
pub async fn diff_filesystem_ex(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    path: Option<RelativePath>,
    filter_mode: FilterMode,
    scan_dirty: bool,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    let link_mounts = Arc::new(collect_link_mounts(&state_current, &repository_current).await?);
    if let Some(path) = path {
        let excluded = repository_from
            .filter
            .emit_excludes(&path, true, filter_mode);
        if excluded {
            return Ok((Vec::new(), FilesystemDiffStats::default()));
        }

        let node_link_from = state_from
            .find_node_link(repository_from.clone(), path.as_str())
            .await
            .unwrap_or(NodeLink {
                node: INVALID_NODE,
                repository: repository_from.id,
                revision: state_from.revision(),
            });
        let (repository_from, state_from) = node_link_from
            .resolve(repository_from.clone(), state_from.clone())
            .await?;

        let node_link_to = state_current
            .find_node_link(repository_current.clone(), path.as_str())
            .await
            .unwrap_or(NodeLink {
                node: INVALID_NODE,
                repository: repository_current.id,
                revision: state_current.revision(),
            });
        let (repository_current, state_current) = node_link_to
            .resolve(repository_current.clone(), state_current.clone())
            .await?;

        diff_filesystem_subtree_impl(DiffFilesystemContext {
            from: FilesystemTraversal {
                repository: repository_from,
                state: state_from,
                node_path: path.clone(),
                root_node: node_link_from.node,
            },
            current: FilesystemTraversal {
                repository: repository_current,
                state: state_current,
                node_path: path.clone(),
                root_node: node_link_to.node,
            },
            filesystem_path: path,
            filter_mode,
            scan_dirty,
            layer_mounts,
            link_mounts,
        })
        .await
    } else {
        diff_filesystem_subtree_impl(DiffFilesystemContext {
            from: FilesystemTraversal {
                repository: repository_from,
                state: state_from,
                node_path: RelativePath::new(),
                root_node: ROOT_NODE,
            },
            current: FilesystemTraversal {
                repository: repository_current,
                state: state_current,
                node_path: RelativePath::new(),
                root_node: ROOT_NODE,
            },
            filesystem_path: RelativePath::new(),
            filter_mode,
            scan_dirty,
            layer_mounts,
            link_mounts,
        })
        .await
    }
}

/// Patch-discard reverted-DirtyAdd nodes collected during the parallel
/// filesystem walk and clear stale `Dirty` propagation on each ancestor
/// chain. Must only be called after the corresponding walk's task set has
/// drained — discarding mid-walk mutates `parent.child` / sibling chains
/// under walks that are still reading them and races into
/// `node_discard_patch`'s `"Discard hierarchy broken"`.
async fn apply_pending_discards(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    mut pending_discards: Vec<NodeID>,
) -> Result<(), StateError> {
    if pending_discards.is_empty() {
        return Ok(());
    }
    pending_discards.sort_unstable();
    pending_discards.dedup();

    for discard_node_id in pending_discards {
        let Ok(discard_node) = state.node(repository.clone(), discard_node_id).await else {
            continue;
        };
        if discard_node.is_discarded() {
            continue;
        }

        let initial_ancestor = discard_node.parent;
        node_discard_patch(
            state.clone(),
            repository.clone(),
            discard_node_id,
            |_, _| {},
        )
        .await?;
        state.mark_dirty();

        let mut ancestor_node_id = initial_ancestor;
        while ancestor_node_id.is_valid_node_id() {
            if state
                .node_has_dirty_children(repository.clone(), ancestor_node_id)
                .await?
            {
                break;
            }
            let ancestor_block_index = NodeBlock::index(ancestor_node_id);
            let ancestor_node_index = Node::index(ancestor_node_id);
            let ancestor_block = state
                .block(repository.clone(), ancestor_block_index)
                .await?;
            let next_ancestor_node_id = ancestor_block.node(ancestor_node_index).parent;
            let block_dirtied = {
                let mut block_writer = ancestor_block.write();
                block_writer.node(ancestor_node_index).clear_dirty_flags();
                block_writer.mark_dirty()
            };
            if block_dirtied {
                state.block_modified(ancestor_block, ancestor_block_index);
                state.mark_dirty();
            }
            if ancestor_node_id == ROOT_NODE {
                break;
            }
            ancestor_node_id = next_ancestor_node_id;
        }
    }
    Ok(())
}

struct FilesystemTraversal {
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_path: RelativePath,
    root_node: NodeID,
}

struct DiffFilesystemContext {
    from: FilesystemTraversal,
    current: FilesystemTraversal,
    filesystem_path: RelativePath,
    filter_mode: FilterMode,
    scan_dirty: bool,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
    link_mounts: Arc<Vec<LinkMountInfo>>,
}

/// Calculate the set of changes from state to filesystem for a subsection of the tree.
/// This is the main entry point that dispatches to file or directory handling.
#[allow(clippy::too_many_arguments)]
pub async fn diff_filesystem_subtree(
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    node_path: RelativePath,
    root_node_from: NodeID,
    root_node_to: NodeID,
    filter_mode: FilterMode,
    layer_mounts: Arc<Vec<LayerMountInfo>>,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    let link_mounts = Arc::new(collect_link_mounts(&state_current, &repository_current).await?);
    diff_filesystem_subtree_impl(DiffFilesystemContext {
        from: FilesystemTraversal {
            repository: repository_from,
            state: state_from,
            node_path: node_path.clone(),
            root_node: root_node_from,
        },
        current: FilesystemTraversal {
            repository: repository_current,
            state: state_current,
            node_path: node_path.clone(),
            root_node: root_node_to,
        },
        filesystem_path: node_path,
        filter_mode,
        scan_dirty: false,
        layer_mounts,
        link_mounts,
    })
    .await
}

/// Find-or-create the directory node chain from `ROOT_NODE` down to `path`,
/// marking newly created segments as dirty-add. A path-filtered scan can enter
/// a directory (or a file's parent) present on disk but absent from `state_from`;
/// creating the chain lets adds discovered inside resolve their parent node.
/// Returns the node for the final path segment.
async fn ensure_scan_dir_chain(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path: &str,
) -> Result<NodeID, StateError> {
    let mut current_node = ROOT_NODE;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        let name_hash = crate::hash::hash_string(segment);
        if let Ok(child_id) = state
            .find_subnode(repository.clone(), current_node, name_hash)
            .await
        {
            current_node = child_id;
        } else {
            let dir_node = Node {
                flags: NodeFlags::DirtyAdd.bits(),
                name_hash,
                ..Default::default()
            };
            current_node = state
                .node_add(repository.clone(), current_node, dir_node, segment)
                .await
                .forward::<StateError>("scan add: failed to create entry directory node")?;
        }
    }
    Ok(current_node)
}

async fn diff_filesystem_subtree_impl(
    mut ctx: DiffFilesystemContext,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    let absolute_path = ctx
        .filesystem_path
        .to_absolute_path(ctx.from.repository.require_path()?);

    match util::fs::list_path(absolute_path) {
        util::fs::PathListingResult::Directory { receiver } => {
            // A path-filtered scan can enter a directory present on disk but
            // absent from state_from (an untracked add). Create its dirty-add
            // node chain so adds discovered inside resolve their parent node.
            if ctx.scan_dirty
                && !ctx.from.root_node.is_valid_or_root_node_id()
                && !ctx.filesystem_path.is_empty()
            {
                let entry_node = ensure_scan_dir_chain(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    ctx.filesystem_path.as_str(),
                )
                .await?;
                ctx.from.root_node = entry_node;
            }
            diff_filesystem_directory(ctx, receiver).await
        }
        util::fs::PathListingResult::File { item } => {
            // A path-filtered scan of a new file: ensure its parent directory
            // chain exists so the add resolves its parent node.
            if ctx.scan_dirty
                && !ctx.from.root_node.is_valid_node_id()
                && let Some(parent) = ctx.filesystem_path.parent()
                && !parent.is_empty()
            {
                ensure_scan_dir_chain(ctx.from.repository.clone(), ctx.from.state.clone(), parent)
                    .await?;
            }
            diff_filesystem_single_file(ctx, item).await
        }
        util::fs::PathListingResult::NotFound => {
            // Path doesn't exist on filesystem - everything in state is deleted
            diff_filesystem_missing(
                ctx.from,
                ctx.filesystem_path,
                ctx.filter_mode,
                ctx.scan_dirty,
            )
            .await
        }
    }
}

/// Result of comparing a single file from filesystem against state.
/// This captures the common logic for single-file node comparison.
#[derive(Debug)]
pub enum SingleFileCompareResult {
    /// File is unmodified (content matches state)
    Unmodified,
    /// File is modified (content differs from state)
    Modified,
    /// File is new (not present in state)
    NewFile,
    /// Type changed from directory/link to file
    TypeChangedToFile,
    /// Type changed from file to directory
    TypeChangedToDirectory,
}

/// Compare a single file from filesystem against state and determine the type of change.
/// This is a pure comparison function that doesn't create changes - it just determines
/// what kind of change (if any) occurred.
///
/// # Arguments
/// * `repository` - Repository context
/// * `from_node` - The state node to compare against (None if file is new)
/// * `current_node` - The current state node (for timestamp tracking comparison)
/// * `file_metadata` - Filesystem metadata for the file
/// * `file_path` - Path to the file (relative)
/// * `is_filesystem_file` - Whether the filesystem path is a file (vs directory)
///
/// # Returns
/// The comparison result indicating what type of change occurred
async fn compare_single_file_against_state(
    repository: Arc<RepositoryContext>,
    from_node: Option<&Node>,
    current_node: Option<&Node>,
    file_metadata: &std::fs::Metadata,
    file_path: &RelativePath,
) -> Result<SingleFileCompareResult, StateError> {
    let Some(from_node) = from_node else {
        // No state node - this is a new file
        return Ok(SingleFileCompareResult::NewFile);
    };

    let state_is_file = from_node.is_file();
    let _state_is_directory = from_node.is_directory();
    let _state_is_link = from_node.is_link();

    // Handle type changes
    let filesystem_is_file = file_metadata.is_file();
    if filesystem_is_file && !state_is_file {
        // Filesystem has file, state has directory or link
        return Ok(SingleFileCompareResult::TypeChangedToFile);
    }

    if !filesystem_is_file && state_is_file {
        // Filesystem has directory, state has file
        return Ok(SingleFileCompareResult::TypeChangedToDirectory);
    }

    // At this point, both are files - check for modifications
    if state_is_file && filesystem_is_file {
        // Force hash check if the from state doesn't match current state
        // (timestamp tracking only tells us if file matches what was last written,
        // which is the current state)
        let force_hash_check =
            current_node.is_none_or(|n| n.address.hash != from_node.address.hash);

        let (file_mtime, file_size) = util::fs::file_mtime_and_size(file_metadata);
        let (modified, _) = is_file_modified(
            repository,
            from_node,
            file_mtime,
            file_size,
            file_path,
            force_hash_check,
        )
        .await?;

        if modified {
            return Ok(SingleFileCompareResult::Modified);
        }
    }

    Ok(SingleFileCompareResult::Unmodified)
}

/// Context for creating file diff changes.
/// Encapsulates all the state needed to create `NodeChangeState` instances.
struct FileDiffContext {
    repository_from: Arc<RepositoryContext>,
    state_from: Arc<State>,
    from_node_id: NodeID,
    from_node: Option<Node>,
    /// Parent directory node for a newly added leaf. `Some` from the directory
    /// walk (the directory being walked), which is correct even across a
    /// link/layer boundary; `None` for a single-file path, resolved by path.
    parent_node_id: Option<NodeID>,
    /// When true, set Dirty on modified files and clear Dirty on retained files inline.
    scan_dirty: bool,
}

impl FileDiffContext {
    /// Create a `NodeChangeState` for the 'from' side of a change.
    fn create_from_change_state(&self) -> NodeChangeState {
        NodeChangeState {
            repository: self.repository_from.clone(),
            state: self.state_from.clone(),
            node: self.from_node_id,
            flags: self
                .from_node
                .map_or(NodeFlags::NoFlags, |n| NodeFlags::from_bits_retain(n.flags)),
            address: self.from_node.map_or_else(Address::default, |n| n.address),
        }
    }

    /// Create a `NodeChangeState` representing an invalid/empty state.
    fn invalid_change_state(&self) -> NodeChangeState {
        NodeChangeState {
            repository: self.repository_from.clone(),
            state: self.state_from.clone(),
            node: INVALID_NODE,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
        }
    }

    /// Create a `NodeChangeState` for a new file (filesystem path not in state).
    fn new_file_change_state(&self) -> NodeChangeState {
        NodeChangeState {
            repository: self.repository_from.clone(),
            state: self.state_from.clone(),
            node: INVALID_NODE,
            flags: NodeFlags::File,
            address: Address::default(),
        }
    }

    /// Create a `NodeChangeState` for a new directory (filesystem path not in state).
    fn new_directory_change_state(&self) -> NodeChangeState {
        NodeChangeState {
            repository: self.repository_from.clone(),
            state: self.state_from.clone(),
            node: INVALID_NODE,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
        }
    }
}

/// Emit an Add+Dirty reconciliation change for a file whose node exists in
/// `state_from` (staged) but not in the current state. The file's presence
/// on disk is the add, and the node carries the `DirtyAdd` flag (re-marked
/// here if it was cleared by stale reconciliation). The compare framework
/// is bypassed because comparing the filesystem hash against the staged
/// node's zero address is meaningless for an add.
#[allow(clippy::too_many_arguments)]
async fn emit_unstaged_add(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    from_node_id: NodeID,
    from_node: Node,
    file_path: &RelativePath,
    sink: &mut ChangeSink<'_>,
    stats: &FilesystemDiffStats,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    if !from_node.is_dirty_add() {
        state
            .node_mark_dirty(repository.clone(), from_node_id, NodeFlags::DirtyAdd, true)
            .await?;
    }
    let block_index = NodeBlock::index(from_node_id);
    let node_index = Node::index(from_node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let node = block.node(node_index);
    add_change(
        NodeChangeState {
            repository: repository.clone(),
            state: state.clone(),
            node: INVALID_NODE,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
        },
        NodeChangeState {
            repository: repository.clone(),
            state: state.clone(),
            node: from_node_id,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
        },
        change::FileAction::Add,
        file_path,
        None,
        sink,
        filter_mode,
    )
    .await?;
    stats.file_add.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Emit a single Add change for `node_id` without recursing into its subtree —
/// the caller's walk recursion surfaces the children. Used to report a dirty-add
/// directory exactly once per scan (unlike `add_change`, which recurses the whole
/// hierarchy for a directory add and would double-count against the recursion).
async fn emit_dirty_add_node_single(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    sink: &mut ChangeSink<'_>,
    stats: &FilesystemDiffStats,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    if !node.is_dirty_add() {
        state
            .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyAdd, true)
            .await?;
    }
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    sink.emit(NodeChange {
        action: change::FileAction::Add,
        flags: compute_change_flags(&node, change::FileAction::Add, true),
        from: NodeChangeState {
            repository: repository.clone(),
            state: state.clone(),
            node: INVALID_NODE,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
        },
        to: NodeChangeState {
            repository: repository.clone(),
            state: state.clone(),
            node: node_id,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
        },
        path: path.clone(),
        from_path: None,
    })
    .await?;
    stats.file_add.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Handle the result of a single file comparison and create appropriate changes.
///
/// This is the unified code path for handling single file node changes in both
/// `diff_filesystem_single_file` and `diff_filesystem_directory`.
///
/// # Arguments
/// * `ctx` - Context containing state references for creating changes
/// * `compare_result` - Result of the file comparison
/// * `file_path` - Path to the file (relative)
/// * `from_path` - Original path for rename detection (None if not a rename)
/// * `is_filesystem_directory` - True if the filesystem item is a directory
/// * `changes` - Vector to append changes to
/// * `stats` - Statistics to update
///
/// # Rename Handling
/// When `from_path` is Some, this indicates the file was renamed. The function handles
/// renames for both modified and unmodified content:
/// - Unmodified + Rename: Generates a Move action (file content matches but name changed)
/// - Modified + Rename: Generates a Move action with modified content
#[allow(clippy::too_many_arguments)]
async fn handle_single_file_compare_result(
    ctx: &FileDiffContext,
    compare_result: SingleFileCompareResult,
    file_path: &RelativePath,
    from_path: Option<&RelativePath>,
    is_filesystem_directory: bool,
    sink: &mut ChangeSink<'_>,
    stats: &FilesystemDiffStats,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    match compare_result {
        SingleFileCompareResult::Unmodified => {
            // Handle rename case: content is unchanged but filename differs
            if let Some(original_path) = from_path {
                lore_trace!(
                    "File {} renamed from {}, content unmodified, add move change",
                    file_path,
                    original_path
                );
                add_change(
                    ctx.create_from_change_state(),
                    ctx.new_file_change_state(),
                    change::FileAction::Move,
                    file_path,
                    from_path,
                    sink,
                    filter_mode,
                )
                .await?;
                stats.file_replace.fetch_add(1, Ordering::Relaxed);
            } else {
                lore_trace!("File {} unmodified, retain", file_path);
                stats.file_retain.fetch_add(1, Ordering::Relaxed);

                // Scan: clear stale Dirty on retained file
                if ctx.scan_dirty
                    && ctx.from_node_id.is_valid_node_id()
                    && ctx.from_node.is_some_and(|n| n.is_dirty())
                {
                    ctx.state_from
                        .node_clear_dirty(ctx.repository_from.clone(), ctx.from_node_id)
                        .await?;
                }
            }
        }
        SingleFileCompareResult::Modified => {
            let action = if from_path.is_some() {
                lore_trace!("File {} renamed and modified, add move change", file_path);
                change::FileAction::Move
            } else {
                lore_trace!("File {} modified, add change", file_path);
                change::FileAction::Keep
            };

            // Scan: persist Dirty on the modified node before recording the change so
            // compute_change_flags loads the dirty node and includes Dirty in the event.
            if ctx.scan_dirty && ctx.from_node_id.is_valid_node_id() {
                let dirty_flags = if action == change::FileAction::Move {
                    NodeFlags::DirtyMove
                } else {
                    NodeFlags::DirtyModify
                };
                ctx.state_from
                    .node_mark_dirty(
                        ctx.repository_from.clone(),
                        ctx.from_node_id,
                        dirty_flags,
                        true,
                    )
                    .await?;
            }

            add_change(
                ctx.create_from_change_state(),
                ctx.new_file_change_state(),
                action,
                file_path,
                from_path,
                sink,
                filter_mode,
            )
            .await?;

            stats.file_replace.fetch_add(1, Ordering::Relaxed);
        }
        SingleFileCompareResult::NewFile => {
            lore_trace!("File {} is new (not in state)", file_path);

            // Scan: create the Dirty+Add node in state first, then route add_change
            // through its NodeID so compute_change_flags loads it and sets Dirty.
            let to_state = if !is_filesystem_directory && ctx.scan_dirty {
                let parent_path = file_path.parent();
                let file_name = file_path.name();
                // The directory walk supplies the parent node directly (correct
                // even across link/layer boundaries). For a single-file path the
                // parent was created during discovery, so resolving by path must
                // succeed.
                let parent_node_id = if let Some(parent) = ctx.parent_node_id {
                    parent
                } else {
                    match parent_path {
                        Some(p) if !p.is_empty() => {
                            ctx.state_from
                                .find_node_link(ctx.repository_from.clone(), p)
                                .await
                                .forward::<StateError>(
                                    "scan add: parent directory node missing for nested add",
                                )?
                                .node
                        }
                        _ => ROOT_NODE,
                    }
                };

                let node = Node {
                    flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
                    name_hash: crate::hash::hash_string(file_name),
                    ..Default::default()
                };

                let new_node_id = ctx
                    .state_from
                    .node_add(ctx.repository_from.clone(), parent_node_id, node, file_name)
                    .await
                    .unwrap_or(INVALID_NODE);

                // Propagate dirty to parent
                let _ = ctx
                    .state_from
                    .node_mark_dirty(
                        ctx.repository_from.clone(),
                        parent_node_id,
                        NodeFlags::Dirty,
                        false,
                    )
                    .await;

                NodeChangeState {
                    repository: ctx.repository_from.clone(),
                    state: ctx.state_from.clone(),
                    node: new_node_id,
                    flags: NodeFlags::File | NodeFlags::DirtyAdd,
                    address: Address::default(),
                }
            } else if is_filesystem_directory {
                ctx.new_directory_change_state()
            } else {
                ctx.new_file_change_state()
            };

            add_change(
                ctx.invalid_change_state(),
                to_state,
                FileAction::Add,
                file_path,
                None,
                sink,
                filter_mode,
            )
            .await?;

            stats.file_add.fetch_add(1, Ordering::Relaxed);
        }
        SingleFileCompareResult::TypeChangedToFile => {
            lore_trace!(
                "Type changed at {} - state has directory/link, filesystem has file, delete + add",
                file_path
            );

            // Delete the old directory/link
            add_change(
                ctx.create_from_change_state(),
                ctx.invalid_change_state(),
                FileAction::Delete,
                file_path,
                None,
                sink,
                filter_mode,
            )
            .await?;

            // Add the new file
            add_change(
                ctx.invalid_change_state(),
                ctx.new_file_change_state(),
                FileAction::Add,
                file_path,
                None,
                sink,
                filter_mode,
            )
            .await?;
        }
        SingleFileCompareResult::TypeChangedToDirectory => {
            lore_trace!(
                "Type changed at {} - state has file, filesystem has directory, delete + add",
                file_path
            );

            // Delete the old file
            add_change(
                ctx.create_from_change_state(),
                ctx.invalid_change_state(),
                FileAction::Delete,
                file_path,
                None,
                sink,
                filter_mode,
            )
            .await?;

            // Add the new directory
            add_change(
                ctx.invalid_change_state(),
                ctx.new_directory_change_state(),
                FileAction::Add,
                file_path,
                None,
                sink,
                filter_mode,
            )
            .await?;
        }
    }
    Ok(())
}

/// Handle diff for a directory path.
/// All items from receiver are children of `node_path`.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_directory(
    ctx: DiffFilesystemContext,
    file_receiver: tokio::sync::mpsc::UnboundedReceiver<util::fs::FileListItem>,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    async fn collect_node_list(
        traversal: &FilesystemTraversal,
    ) -> Result<StateChildrenNodes, StateError> {
        let FilesystemTraversal {
            repository,
            state,
            root_node: node_id,
            ..
        } = traversal;
        Ok(if node_id.is_valid_or_root_node_id() {
            let node = state.node(repository.clone(), *node_id).await?;
            if node.is_directory() {
                state
                    .collect_children_unsorted(
                        repository.clone(),
                        *node_id,
                        false, /* ignore deleted */
                        true,  /* Traverse links */
                    )
                    .await?
            } else {
                // State has a file where filesystem has directory - treat as delete + add
                // Return state node as single item for delete comparison
                StateChildrenNodes {
                    repository: repository.clone(),
                    state: state.clone(),
                    children: vec![StateNamedNode {
                        node: *node_id,
                        name: node.name_hash,
                    }],
                }
            }
        } else {
            StateChildrenNodes {
                repository: repository.clone(),
                state: state.clone(),
                children: vec![],
            }
        })
    }
    // Collect state node lists (always directory mode here)
    let mut node_list = collect_node_list(&ctx.from).await?;

    let mut current_node_list = collect_node_list(&ctx.current).await?;

    let mut changes: Vec<NodeChange> = vec![];
    let mut tasks = JoinSet::new();
    let mut stats = FilesystemDiffStats::default();
    let mut pending_discards: Vec<NodeID> = Vec::new();

    // TODO(mjansson) Use (radix) sorter on name for scaling to directories with many entries
    named_node_sort(&mut node_list.children);
    named_node_sort(&mut current_node_list.children);

    let mut node_list_found = vec![false; node_list.children.len()];

    // Run the walk in a helper so any `?` early-out still hits the
    // drain below — otherwise the JoinSet drops with subtree-recursion
    // tasks still running, leaking the Arc<RepositoryContext> clones.
    let work_result = diff_filesystem_directory_walk(
        &ctx,
        file_receiver,
        &node_list,
        &current_node_list,
        &mut node_list_found,
        &mut tasks,
        &mut changes,
        &mut stats,
        &mut pending_discards,
    )
    .await;
    let drain_result = lore_drain_tasks!(tasks, StateError::internal("Task failure"));
    work_result?;
    drain_result?;
    apply_pending_discards(
        node_list.state.clone(),
        node_list.repository.clone(),
        pending_discards,
    )
    .await?;
    Ok((changes, stats))
}

/// Emit a single `Delete` change for one node, reloading it so any dirty flags
/// just persisted by [`State::node_mark_dirty`] are reflected in the record.
async fn emit_single_delete(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    path: &RelativePath,
    sink: &mut ChangeSink<'_>,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    let flags = compute_change_flags(&node, FileAction::Delete, false);
    let from = NodeChangeState {
        repository,
        state,
        node: node_id,
        flags: NodeFlags::from_bits_retain(node.flags),
        address: node.address,
    };
    let to = from.invalid();
    sink.emit(NodeChange {
        action: FileAction::Delete,
        flags,
        from,
        to,
        path: path.clone(),
        from_path: None,
    })
    .await
}

/// Emit the buffered ancestor-directory deletes, outermost first, and clear the
/// buffer so sibling subtrees don't re-emit them. When `scan_dirty` is set each
/// directory is marked `DirtyDelete` first so a later bare `stage` (which walks
/// dirty flags rather than rescanning) picks up the directory deletion.
async fn flush_pending_dir_deletes(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    sink: &mut ChangeSink<'_>,
    pending: &mut Vec<(NodeID, RelativePath)>,
    scan_dirty: bool,
) -> Result<(), StateError> {
    for (node_id, path) in std::mem::take(pending) {
        if scan_dirty {
            state
                .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyDelete, true)
                .await?;
        }
        emit_single_delete(state.clone(), repository.clone(), node_id, &path, sink).await?;
    }
    Ok(())
}

/// Walk a revision subtree that is absent from the filesystem and emit `Delete`
/// changes for only the portion that was actually materialized on disk under the
/// active filter, returning whether anything materialized.
///
/// Materialization mirrors clone/checkout discovery: excluded children are
/// pruned (never written), a non-excluded file or link materializes, and a
/// directory materializes when a descendant does or — when it has no children —
/// when the empty directory itself is not excluded.
///
/// A directory can evaluate as "not excluded" only because the filter let the
/// diff descend through it (a view re-inclusion's generated traversal rules, or a
/// glob matching the directory but not content deeper inside it) while nothing
/// under it is in view. Such a directory is never written, so its delete record
/// is buffered in `pending` and emitted only once a materializing descendant
/// proves it existed on disk; if none does, the buffered entry is dropped. This
/// keeps the report from claiming a delete for a directory that was never there.
///
/// When `scan_dirty` is set, every node a delete is emitted for — the
/// materializing leaf and each flushed ancestor directory — is marked
/// `DirtyDelete` so the persisted dirty-tracking state records the deletion at
/// the granularity it is reported. `node_mark_dirty` short-circuits on a node
/// already carrying the base `Dirty` bit (which `DirtyDelete` includes), so a
/// sibling's upward propagation never clobbers a directory's `DirtyDelete`.
#[allow(clippy::too_many_arguments)]
async fn emit_filesystem_subtree_deletes(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    filter_mode: FilterMode,
    scan_dirty: bool,
    sink: &mut ChangeSink<'_>,
    pending: &mut Vec<(NodeID, RelativePath)>,
) -> Result<bool, StateError> {
    // Caller guarantees `node` is not filter-excluded.
    if node.is_file() || node.is_link() {
        flush_pending_dir_deletes(&state, &repository, sink, pending, scan_dirty).await?;
        if scan_dirty {
            state
                .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyDelete, true)
                .await?;
        }
        emit_single_delete(state, repository, node_id, path, sink).await?;
        return Ok(true);
    }

    pending.push((node_id, path.clone()));
    let depth = pending.len();

    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id).await?;
    let mut had_child = false;
    let mut any_materialized = false;
    while let Some((child_id, child_node, child_name)) = children.next().await? {
        had_child = true;
        let child_path = path.push_into_buf(&child_name).freeze();
        // Release the block read lock before recursing (see NodeNameLock docs).
        drop(child_name);
        if repository
            .filter
            .excludes(&child_path, child_node.is_directory(), filter_mode)
        {
            continue;
        }
        if Box::pin(emit_filesystem_subtree_deletes(
            state.clone(),
            repository.clone(),
            child_id,
            &child_node,
            &child_path,
            filter_mode,
            scan_dirty,
            sink,
            pending,
        ))
        .await?
        {
            any_materialized = true;
        }
    }

    if any_materialized {
        // The first materializing descendant already flushed this directory.
        return Ok(true);
    }

    if !had_child && !repository.filter.excludes(path, true, filter_mode) {
        // Empty in-view directory: clone/checkout writes it, so its absence is a
        // real deletion. It is the materializing leaf here, and its own buffered
        // entry (pushed above) is flushed and marked along with its ancestors.
        flush_pending_dir_deletes(&state, &repository, sink, pending, scan_dirty).await?;
        return Ok(true);
    }

    // Nothing under this directory materialized: drop its buffered entry.
    pending.truncate(depth - 1);
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_directory_walk(
    ctx: &DiffFilesystemContext,
    mut file_receiver: tokio::sync::mpsc::UnboundedReceiver<util::fs::FileListItem>,
    node_list: &StateChildrenNodes,
    current_node_list: &StateChildrenNodes,
    node_list_found: &mut [bool],
    tasks: &mut JoinSet<Result<(Vec<NodeChange>, FilesystemDiffStats), StateError>>,
    changes: &mut Vec<NodeChange>,
    stats: &mut FilesystemDiffStats,
    pending_discards: &mut Vec<NodeID>,
) -> Result<(), StateError> {
    let mut new_file_list = vec![];
    while let Some(item) = file_receiver.recv().await {
        if item.name == DOT_URC || item.name == DOT_LORE {
            continue;
        }

        // For directory listing, all items are children - construct child path
        let item_path = ctx
            .filesystem_path
            .push_into_buf(item.name.as_str())
            .freeze();

        if ctx.from.repository.filter.emit_excludes(
            &item_path,
            item.metadata.is_dir(),
            ctx.filter_mode,
        ) {
            continue;
        }

        let current_index = if let Ok(index) = node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&item.name_hash))
        {
            index
        } else {
            new_file_list.push(item);
            continue;
        };

        let from_named_node = &node_list.children[current_index];
        node_list_found[current_index] = true;

        let (current_node, current_node_id, current_path) = match current_node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&item.name_hash))
        {
            Ok(index) => {
                let current_node_id = current_node_list.children[index].node;
                if let Some(search) =
                    get_node_and_path(current_node_list, current_node_id, &ctx.current.node_path)
                        .await?
                {
                    (search.node, current_node_id, search.path)
                } else {
                    (Node::default(), INVALID_NODE, RelativePath::new())
                }
            }
            Err(_) => (Node::default(), INVALID_NODE, RelativePath::new()),
        };

        // Check if modified
        let Some(NodeSearchResult {
            node: from_node,
            path: from_path,
        }) = get_node_and_path(node_list, from_named_node.node, &ctx.from.node_path).await?
        else {
            continue;
        };

        let was_file = from_node.is_file();
        let was_directory = from_node.is_directory();
        let was_link = from_node.is_link();

        let is_directory = item.metadata.is_dir();
        let is_file = item.metadata.is_file();

        let from_node_name = from_path.name();
        let is_rename = *item.name != *from_node_name;

        if was_file && is_file {
            // A node in state_from but not in state_current is an unstaged
            // add — the file's presence on disk is the add. Comparing the
            // filesystem hash against the staged node's zero address would
            // misclassify, so emit Add+Dirty directly and skip the compare.
            if ctx.scan_dirty && !current_node_id.is_valid_node_id() {
                emit_unstaged_add(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    from_node,
                    &item_path,
                    &mut ChangeSink::Vec(&mut *changes),
                    stats,
                    ctx.filter_mode,
                )
                .await?;
                continue;
            }

            let current_node_ref = if current_node_id.is_valid_node_id() {
                Some(&current_node)
            } else {
                None
            };

            let compare_result = compare_single_file_against_state(
                node_list.repository.clone(),
                Some(&from_node),
                current_node_ref,
                &item.metadata,
                &item_path,
            )
            .await?;

            // Create context for generating changes
            let file_ctx = FileDiffContext {
                repository_from: node_list.repository.clone(),
                state_from: node_list.state.clone(),
                from_node_id: from_named_node.node,
                from_node: Some(from_node),
                parent_node_id: Some(ctx.from.root_node),
                scan_dirty: ctx.scan_dirty,
            };

            // This handles renames (via from_path_for_rename), modifications, and unmodified cases
            handle_single_file_compare_result(
                &file_ctx,
                compare_result,
                &item_path,
                if is_rename { Some(&from_path) } else { None },
                false, // filesystem item is a file, not directory
                &mut ChangeSink::Vec(&mut *changes),
                stats,
                ctx.filter_mode,
            )
            .await?;
        } else if was_link && is_directory {
            let link = from_node.linked_node();
            let (link_from, state_from) = link
                .resolve(ctx.from.repository.clone(), ctx.from.state.clone())
                .await?;
            let subnode_from = link.node;

            let (link_current, state_current, subnode_current) = if current_node.is_link() {
                let link = current_node.linked_node();
                let (linked_repository, state) = link
                    .resolve(ctx.current.repository.clone(), ctx.current.state.clone())
                    .await?;
                (linked_repository, state, link.node)
            } else {
                // Current state has no matching link (staged-add link or link replacing
                // a non-link in current). Use the from-side linked state for both sides
                // so files already tracked in the linked tree aren't misclassified as
                // unstaged adds.
                (link_from.clone(), state_from.clone(), subnode_from)
            };
            let subpath = item_path.clone();
            let layer_mounts_recurse = ctx.layer_mounts.clone();
            let filter_mode = ctx.filter_mode;
            let scan_dirty = ctx.scan_dirty;
            lore_spawn!(tasks, async move {
                diff_filesystem_subtree_recurse(DiffFilesystemContext {
                    from: FilesystemTraversal {
                        repository: link_from,
                        state: state_from,
                        node_path: from_path,
                        root_node: subnode_from,
                    },
                    current: FilesystemTraversal {
                        repository: link_current,
                        state: state_current,
                        node_path: current_path,
                        root_node: subnode_current,
                    },
                    filesystem_path: subpath,
                    filter_mode,
                    scan_dirty,
                    layer_mounts: layer_mounts_recurse,
                    // Crossing into the linked state; parent's link mounts
                    // are paths in the parent tree and do not apply here.
                    link_mounts: Arc::new(vec![]),
                })
                .await
            });
        } else if was_directory && is_directory {
            if ctx.scan_dirty && !current_node_id.is_valid_node_id() {
                // Re-emit a staged dirty-add directory (in staged, absent from
                // the current revision) as a single node so repeated scans stay
                // idempotent; the recursion below surfaces its children.
                emit_dirty_add_node_single(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    &item_path,
                    &mut ChangeSink::Vec(&mut *changes),
                    stats,
                )
                .await?;
            } else if is_rename {
                add_change(
                    NodeChangeState {
                        repository: node_list.repository.clone(),
                        state: node_list.state.clone(),
                        node: from_named_node.node,
                        flags: NodeFlags::from_bits_retain(from_node.flags),
                        address: from_node.address,
                    },
                    NodeChangeState {
                        repository: current_node_list.repository.clone(),
                        state: current_node_list.state.clone(),
                        node: current_node_id,
                        flags: NodeFlags::from_bits_retain(current_node.flags),
                        address: current_node.address,
                    },
                    FileAction::Move,
                    &item_path,
                    Some(&from_path),
                    &mut ChangeSink::Vec(&mut *changes),
                    ctx.filter_mode,
                )
                .await?;
            }
            let subpath = item_path.clone();
            let repository_from = node_list.repository.clone();
            let state_from = node_list.state.clone();
            let repository_current = current_node_list.repository.clone();
            let state_current = current_node_list.state.clone();
            let subnode_from = from_named_node.node;
            let current_is_link = current_node.is_link();
            let (repository_current, state_current, subnode_current) = if current_is_link {
                let link = current_node.linked_node();
                let (linked_repository, state) = link
                    .resolve(repository_current.clone(), state_current.clone())
                    .await?;
                (linked_repository, state, link.node)
            } else {
                (repository_current, state_current.clone(), current_node_id)
            };
            let layer_mounts_recurse = ctx.layer_mounts.clone();
            // Stay in the parent's link mounts when recursing into a normal
            // sub-directory; reset when crossing into a linked state because
            // those mount paths are in the parent tree, not the linked tree.
            let link_mounts_recurse = if current_is_link {
                Arc::new(vec![])
            } else {
                ctx.link_mounts.clone()
            };
            let filter_mode = ctx.filter_mode;
            let scan_dirty = ctx.scan_dirty;
            lore_spawn!(tasks, async move {
                diff_filesystem_subtree_recurse(DiffFilesystemContext {
                    from: FilesystemTraversal {
                        repository: repository_from,
                        state: state_from,
                        node_path: from_path,
                        root_node: subnode_from,
                    },
                    current: FilesystemTraversal {
                        repository: repository_current,
                        state: state_current,
                        node_path: current_path,
                        root_node: subnode_current,
                    },
                    filesystem_path: subpath,
                    filter_mode,
                    scan_dirty,
                    layer_mounts: layer_mounts_recurse,
                    link_mounts: link_mounts_recurse,
                })
                .await
            });
        } else {
            // Type change: file <-> directory
            let file_ctx = FileDiffContext {
                repository_from: node_list.repository.clone(),
                state_from: node_list.state.clone(),
                from_node_id: from_named_node.node,
                from_node: Some(from_node),
                parent_node_id: Some(ctx.from.root_node),
                scan_dirty: ctx.scan_dirty,
            };

            // Determine the type change direction
            let compare_result = if is_file {
                SingleFileCompareResult::TypeChangedToFile
            } else {
                SingleFileCompareResult::TypeChangedToDirectory
            };

            lore_trace!(
                "Filesystem type (file/directory) differs for node {} in path {}, add delete and add changes",
                from_named_node.node,
                item_path
            );

            handle_single_file_compare_result(
                &file_ctx,
                compare_result,
                &item_path,
                None,
                is_directory,
                &mut ChangeSink::Vec(&mut *changes),
                stats,
                ctx.filter_mode,
            )
            .await?;
        }
    }

    // Nodes that were not iterated are deleted in file system
    for (index, from_named_node) in node_list.children.iter().enumerate() {
        if node_list_found[index] {
            continue;
        }

        let Some(from_node) = get_filtered_node_and_path(
            node_list,
            from_named_node.node,
            &ctx.from.node_path,
            ctx.filter_mode,
        )
        .await?
        else {
            continue;
        };

        // Emit deletes only for the materialized portion of the subtree,
        // suppressing directories the filter merely descended through but never
        // wrote to disk (see emit_filesystem_subtree_deletes).
        if from_node.node.is_directory() {
            let mut pending = Vec::new();
            emit_filesystem_subtree_deletes(
                node_list.state.clone(),
                node_list.repository.clone(),
                from_named_node.node,
                &from_node.node,
                &from_node.path,
                ctx.filter_mode,
                ctx.scan_dirty,
                &mut ChangeSink::Vec(&mut *changes),
                &mut pending,
            )
            .await?;
            continue;
        }

        // A leaf node present in state_from but not in state_current, with
        // no file on disk, is an unstaged add that the user reverted by
        // removing the file. Discard the node so state_staged matches the
        // filesystem rather than emitting a Delete change for a node that
        // shouldn't exist.
        let in_current = current_node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&from_named_node.name))
            .is_ok();
        if ctx.scan_dirty && from_node.node.is_file() && !in_current {
            lore_trace!(
                "Queueing reverted-DirtyAdd node {} (no file at {}, not in current)",
                from_named_node.node,
                from_node.path
            );
            pending_discards.push(from_named_node.node);
            continue;
        }

        // Scan: persist Dirty+Delete on the missing node before recording the change
        // so compute_change_flags loads the dirty node and includes Dirty in the event.
        if ctx.scan_dirty {
            node_list
                .state
                .node_mark_dirty(
                    node_list.repository.clone(),
                    from_named_node.node,
                    NodeFlags::DirtyDelete,
                    true,
                )
                .await?;
        }

        lore_trace!(
            "Filesystem does not have node {} in path {}, add deleted change",
            from_named_node.node,
            ctx.filesystem_path
        );

        add_change(
            NodeChangeState {
                repository: node_list.repository.clone(),
                state: node_list.state.clone(),
                node: from_named_node.node,
                flags: NodeFlags::from_bits_retain(from_node.node.flags),
                address: from_node.node.address,
            },
            NodeChangeState {
                repository: node_list.repository.clone(),
                state: node_list.state.clone(),
                node: INVALID_NODE,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
            },
            FileAction::Delete,
            &from_node.path,
            None,
            &mut ChangeSink::Vec(&mut *changes),
            ctx.filter_mode,
        )
        .await?;
    }

    // Remaining files/directories are added (all are children of node_path)
    'new_file_iter: for file in new_file_list.iter() {
        // For directory listing, new items are children
        let child_file_path = ctx
            .filesystem_path
            .push_into_buf(file.name.as_str())
            .freeze();

        if ctx.from.repository.filter.emit_excludes(
            &child_file_path,
            file.metadata.is_dir(),
            ctx.filter_mode,
        ) {
            continue 'new_file_iter;
        }

        let is_directory = file.metadata.is_dir();

        if is_directory {
            // A directory on disk with no `state_from` node that matches a
            // link in `state_current` is a link add, not a per-file add: the
            // mounted content belongs to the linked repository. Skip the
            // entry; the link node is reported via `state::diff_collect` (in
            // `lore status`) and `link list`, not via `file diff`.
            //
            // The realistic scan-side caller (`lore status` via
            // `diff_filesystem_ex`) stages the link in `state_from` before
            // status runs, so the link is matched in the paired
            // `was_link && is_directory` branch above and never reaches here;
            // the `continue` fires with `scan_dirty == true` only in a
            // constructed corner case, where skipping dirty-add is still
            // correct (the link is not new in the working state).
            if ctx
                .link_mounts
                .iter()
                .any(|m| m.target_path == child_file_path.as_str())
            {
                lore_trace!(
                    "Filesystem path {child_file_path} matches a link in the current state, skipping link-internal content"
                );
                continue 'new_file_iter;
            }
            // Layer mount detection: if this directory's parent-relative path
            // matches a configured layer mount, switch comparison context to
            // the layer's repo and state for the recursion. The layer mount
            // itself is NOT emitted as an "add" — its content is owned by the
            // layer's pinned revision, not the parent's tree.
            if let Some(mount) = ctx
                .layer_mounts
                .iter()
                .find(|m| m.target_path == child_file_path.as_str())
            {
                lore_trace!(
                    "Filesystem path {child_file_path} is a layer mount, recursing into layer state"
                );
                let layer_repository = mount.repository.clone();
                let layer_state = mount.state.clone();
                let subpath = child_file_path.clone();
                let layer_source_node = mount.source_node;
                lore_spawn!(
                    tasks,
                    diff_filesystem_subtree_recurse(DiffFilesystemContext {
                        from: FilesystemTraversal {
                            repository: layer_repository.clone(),
                            state: layer_state.clone(),
                            node_path: subpath.clone(),
                            root_node: layer_source_node,
                        },
                        current: FilesystemTraversal {
                            repository: layer_repository,
                            state: layer_state,
                            node_path: subpath.clone(),
                            root_node: layer_source_node,
                        },
                        filesystem_path: subpath,
                        filter_mode: ctx.filter_mode,
                        scan_dirty: ctx.scan_dirty,
                        // Non-overlapping layers: no nested mounts inside a layer.
                        layer_mounts: Arc::new(vec![]),
                        // Crossing into the layer state; parent's link mounts
                        // are paths in the parent tree and do not apply here.
                        link_mounts: Arc::new(vec![]),
                    },)
                );
                continue 'new_file_iter;
            }
            lore_trace!("Filesystem has new directory in path {child_file_path}, recursing");

            // Scan: persist a Dirty+Add node for the new directory before
            // recursing, so files inside it resolve their parent and the staged
            // anchor rebase can descend the dirty subtree. Emit it as a single
            // node (the recursion below surfaces the children) and recurse
            // against it so a rescan matches the persisted subtree.
            let mut dir_from_root = INVALID_NODE;
            let mut dir_from_path = RelativePath::new();
            if ctx.scan_dirty {
                // The new directory is a child of the directory currently being
                // walked; its node is the correct parent even across link/layer
                // boundaries (resolving by parent path would not match there).
                let dir_parent_node = ctx.from.root_node;
                let dir_node = Node {
                    flags: NodeFlags::DirtyAdd.bits(),
                    name_hash: crate::hash::hash_string(file.name.as_str()),
                    ..Default::default()
                };
                let new_dir_id = ctx
                    .from
                    .state
                    .node_add(
                        ctx.from.repository.clone(),
                        dir_parent_node,
                        dir_node,
                        file.name.as_str(),
                    )
                    .await
                    .forward::<StateError>("scan add: failed to add new directory node")?;
                emit_dirty_add_node_single(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    new_dir_id,
                    &child_file_path,
                    &mut ChangeSink::Vec(&mut *changes),
                    stats,
                )
                .await?;
                ctx.from
                    .state
                    .node_mark_dirty(
                        ctx.from.repository.clone(),
                        dir_parent_node,
                        NodeFlags::Dirty,
                        false,
                    )
                    .await?;
                dir_from_root = new_dir_id;
                dir_from_path = child_file_path.clone();
            }

            let repository_from = ctx.from.repository.clone();
            let state_from = ctx.from.state.clone();
            let repository_current = ctx.current.repository.clone();
            let state_current = ctx.current.state.clone();
            let subpath = child_file_path.clone();
            lore_spawn!(
                tasks,
                diff_filesystem_subtree_recurse(DiffFilesystemContext {
                    from: FilesystemTraversal {
                        repository: repository_from,
                        state: state_from,
                        node_path: dir_from_path,
                        root_node: dir_from_root,
                    },
                    current: FilesystemTraversal {
                        repository: repository_current,
                        state: state_current,
                        node_path: RelativePath::new(),
                        root_node: INVALID_NODE,
                    },
                    filesystem_path: subpath,
                    filter_mode: ctx.filter_mode,
                    scan_dirty: ctx.scan_dirty,
                    layer_mounts: ctx.layer_mounts.clone(),
                    // Same parent state; deeper paths may still match a link.
                    link_mounts: ctx.link_mounts.clone(),
                })
            );

            // The single Dirty+Add directory node emitted above is the scan's
            // report for this new directory; skip the transient change below.
            if ctx.scan_dirty {
                continue 'new_file_iter;
            }
        }

        let file_ctx = FileDiffContext {
            repository_from: ctx.from.repository.clone(),
            state_from: ctx.from.state.clone(),
            from_node_id: INVALID_NODE,
            from_node: None,
            parent_node_id: Some(ctx.from.root_node),
            scan_dirty: ctx.scan_dirty,
        };

        lore_trace!("Filesystem has new item in path {child_file_path}, add add change");

        handle_single_file_compare_result(
            &file_ctx,
            SingleFileCompareResult::NewFile,
            &child_file_path,
            None,
            is_directory,
            &mut ChangeSink::Vec(&mut *changes),
            stats,
            ctx.filter_mode,
        )
        .await?;
    }

    while let Some(task_result) = tasks.join_next().await {
        let (mut task_changes, task_stats) = task_result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()?;
        changes.append(&mut task_changes);
        stats.append(task_stats);
    }

    Ok(())
}

/// Handle diff for a single file path.
/// The item is the file at `node_path` (not a child).
///
/// This function uses the unified single-file comparison logic via
/// `compare_single_file_against_state` and `handle_single_file_compare_result`.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_single_file(
    ctx: DiffFilesystemContext,
    file_item: util::fs::FileListItem,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    let mut changes = vec![];
    let stats = FilesystemDiffStats::default();

    // Path is already correct - file_item represents node_path itself
    // No path manipulation needed!

    // Get the state nodes for comparison
    let from_node = if ctx.from.root_node.is_valid_node_id() {
        ctx.from
            .state
            .node(ctx.from.repository.clone(), ctx.from.root_node)
            .await
            .ok()
    } else {
        None
    };

    let current_node = if ctx.current.root_node.is_valid_node_id() {
        ctx.current
            .state
            .node(ctx.current.repository.clone(), ctx.current.root_node)
            .await
            .ok()
    } else {
        None
    };

    // A node in state_from but not in state_current is an unstaged add —
    // the file's presence on disk is the add. Skip the compare and emit
    // Add+Dirty directly.
    if ctx.scan_dirty
        && file_item.metadata.is_file()
        && ctx.from.root_node.is_valid_node_id()
        && !ctx.current.root_node.is_valid_node_id()
        && let Some(node) = from_node
        && node.is_file()
    {
        emit_unstaged_add(
            ctx.from.repository.clone(),
            ctx.from.state.clone(),
            ctx.from.root_node,
            node,
            &ctx.filesystem_path,
            &mut ChangeSink::Vec(&mut changes),
            &stats,
            ctx.filter_mode,
        )
        .await?;
        return Ok((changes, stats));
    }

    let compare_result = compare_single_file_against_state(
        ctx.from.repository.clone(),
        from_node.as_ref(),
        current_node.as_ref(),
        &file_item.metadata,
        &ctx.filesystem_path,
    )
    .await?;

    // Create the context for generating changes
    let file_ctx = FileDiffContext {
        repository_from: ctx.from.repository.clone(),
        state_from: ctx.from.state.clone(),
        from_node_id: ctx.from.root_node,
        from_node,
        parent_node_id: None,
        scan_dirty: ctx.scan_dirty,
    };

    handle_single_file_compare_result(
        &file_ctx,
        compare_result,
        &ctx.filesystem_path,
        None, // No rename detection for single file path
        file_item.metadata.is_dir(),
        &mut ChangeSink::Vec(&mut changes),
        &stats,
        ctx.filter_mode,
    )
    .await?;

    Ok((changes, stats))
}

/// Handle diff when filesystem path doesn't exist.
/// Everything in state under this path is considered deleted.
async fn diff_filesystem_missing(
    from: FilesystemTraversal,
    node_path: RelativePath,
    filter_mode: FilterMode,
    scan_dirty: bool,
) -> Result<(Vec<NodeChange>, FilesystemDiffStats), StateError> {
    let mut changes = vec![];
    let stats = FilesystemDiffStats::default();

    // Add delete changes for all nodes under root_node_from
    if from.root_node.is_valid_node_id() {
        let from_node = from
            .state
            .node(from.repository.clone(), from.root_node)
            .await?;

        lore_trace!(
            "Filesystem path {} does not exist, marking state node {} as deleted",
            node_path,
            from.root_node
        );

        // Scan: mark missing file as Dirty+Delete
        if scan_dirty {
            from.state
                .node_mark_dirty(
                    from.repository.clone(),
                    from.root_node,
                    NodeFlags::DirtyDelete,
                    true,
                )
                .await?;
        }

        add_change(
            NodeChangeState {
                repository: from.repository.clone(),
                state: from.state.clone(),
                node: from.root_node,
                flags: NodeFlags::from_bits_retain(from_node.flags),
                address: from_node.address,
            },
            NodeChangeState {
                repository: from.repository,
                state: from.state,
                node: INVALID_NODE,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
            },
            FileAction::Delete,
            &node_path,
            None,
            &mut ChangeSink::Vec(&mut changes),
            filter_mode,
        )
        .await?;
    }

    Ok((changes, stats))
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn diff_filesystem_subtree_recurse(
    ctx: DiffFilesystemContext,
) -> Pin<Box<dyn Future<Output = Result<(Vec<NodeChange>, FilesystemDiffStats), StateError>> + Send>>
{
    Box::pin(async move { diff_filesystem_subtree_impl(ctx).await })
}

// TODO(UCS-13059): Extend with file mode check
/// Content comparison size limit: files larger than this skip the fallback
/// content comparison when hash mismatches occur due to chunking strategy
/// differences.
pub const CONTENT_COMPARE_MAX_SIZE: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Content comparison streaming threshold: files larger than this use
/// streaming comparison instead of loading entire content into memory.
const CONTENT_COMPARE_STREAM_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MiB

pub async fn is_file_modified(
    repository: Arc<RepositoryContext>,
    node: &Node,
    file_mtime: u64,
    file_size: u64,
    file_path: &RelativePath,
    force_check_hash: bool,
) -> Result<(bool, Hash), StateError> {
    // Assume files are identical if size and timestamp match
    let node_size = node.size;
    if file_size != node_size {
        lore_trace!("File {file_path} size changed, modified");
        return Ok((true, Hash::default()));
    }

    let node_mtime = if !force_check_hash {
        file_modified_time(repository.clone(), file_path).await
    } else {
        0
    };
    let mtime_match = file_mtime == node_mtime;

    if !mtime_match {
        lore_trace!(
            "Hash check file {file_path} - file size {file_size} node size {node_size}, file mtime {file_mtime}, node mtime {node_mtime}, force {force_check_hash}"
        );
        let absolute_path = file_path.to_absolute_path(repository.require_path()?);
        let file_hash = immutable::hash_file(
            repository.clone(),
            &absolute_path,
            Some(node.address),
            Some(node.size as usize),
        )
        .await
        .internal("Failed to hash file")
        .map_err(StateError::from);

        if let Ok(file_hash) = file_hash {
            if file_hash == node.address.hash {
                lore_trace!("File {file_path} unmodified, content hash equal to node");
                file_modified_time_store(repository.clone(), file_path, file_mtime).await;
                return Ok((false, file_hash));
            } else if file_size <= CONTENT_COMPARE_MAX_SIZE
                && is_file_content_equal(
                    repository.clone(),
                    node.address,
                    &absolute_path,
                    file_size,
                )
                .await
            {
                lore_trace!(
                    "File {file_path} hash mismatch but content equal (chunking compatibility)"
                );
                file_modified_time_store(repository.clone(), file_path, file_mtime).await;
                return Ok((false, file_hash));
            } else {
                lore_trace!("File {file_path} hash mismatch, modified");
                return Ok((true, file_hash));
            }
        } else {
            lore_trace!("File {file_path} hash failed, consider unmodified");
        }
    } else {
        lore_trace!("File {file_path} unmodified, size {file_size} and mtime {file_mtime} match");
    }

    Ok((false, Hash::default()))
}

/// Compare the actual content of a stored object with a file on disk.
///
/// This handles cases where the hash representation differs due to chunking
/// strategy changes (e.g. threshold change from 64 KiB to 256 KiB) but the
/// underlying content is identical. Uses `immutable::read()` for files up to
/// 4 MiB and streaming comparison for larger files.
pub async fn is_file_content_equal(
    repository: Arc<RepositoryContext>,
    address: Address,
    absolute_path: &std::path::Path,
    file_size: u64,
) -> bool {
    if address.is_zero() {
        return false;
    }

    let options = read_options_from_repository(&repository);

    if file_size <= CONTENT_COMPARE_STREAM_THRESHOLD {
        // Small file: load both into memory and compare
        let stored = immutable::read(repository, address, None, options).await;
        let local = tokio::fs::read(absolute_path).await;
        match (stored, local) {
            (Ok(stored_bytes), Ok(local_bytes)) => stored_bytes.as_ref() == local_bytes.as_slice(),
            _ => false,
        }
    } else {
        // Large file: stream stored content and compare chunk-by-chunk against
        // the file read in matching chunks
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Bytes>(4);
        let repo_clone = repository.clone();
        let stream_handle = lore_spawn!(async move {
            immutable::read_stream(repo_clone, address, options, sender).await
        });

        let file = match tokio::fs::File::open(absolute_path).await {
            Ok(f) => f,
            Err(_) => return false,
        };
        let mut reader = tokio::io::BufReader::new(file);
        let mut equal = true;
        let mut bytes_compared: u64 = 0;

        while let Some(chunk) = receiver.recv().await {
            use tokio::io::AsyncReadExt;
            let mut local_buf = vec![0u8; chunk.len()];
            if reader.read_exact(&mut local_buf).await.is_ok() {
                if chunk.as_ref() != local_buf.as_slice() {
                    equal = false;
                    break;
                }
                bytes_compared += chunk.len() as u64;
            } else {
                equal = false;
                break;
            }
        }

        // Verify the stream completed successfully and we compared the
        // entire file. A failed or partial stream must not be treated as
        // content equality.
        let stream_ok = stream_handle.await.is_ok_and(|r| r.is_ok());
        equal && stream_ok && bytes_compared == file_size
    }
}

pub fn file_modified_time_key(salt: &[u8], instance: InstanceId, path: impl AsRef<str>) -> Hash {
    hash::hash_function_args_slice(
        salt,
        FILE_MTIME,
        instance.data(),
        path.as_ref().to_lowercase().as_bytes(),
    )
}

pub async fn file_modified_time(repository: Arc<RepositoryContext>, path: impl AsRef<str>) -> u64 {
    let path = path.as_ref();
    let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
    let mtime = if let Ok(value) = repository
        .read_mutable_store()
        .load(repository.id, key, KeyType::Untyped)
        .await
    {
        u64::from_ne_bytes(
            value.data()[..size_of::<u64>()]
                .try_into()
                .unwrap_or_default(),
        )
    } else {
        0
    };
    lore_trace!("Load mtime {mtime} for {path}");
    mtime
}

pub async fn file_modified_time_store(
    repository: Arc<RepositoryContext>,
    path: impl AsRef<str>,
    mtime: u64,
) {
    let path = path.as_ref();
    lore_trace!("Store mtime {mtime} for {path}");
    let Some(handle) = repository.try_write_mutable_store() else {
        return;
    };
    let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
    let _ = handle
        .store(repository.id, key, Hash::from_u64(mtime), KeyType::Untyped)
        .await;
}

/// Batch-write a collection of pre-computed `(mtime_key, mtime)` pairs into the
/// mutable store. Used by the clone hot path so each `clone_file` task can drop
/// its mtime into a shared buffer and a single fire-and-forget task issues the
/// store calls instead of every task awaiting its own bucket lock inline.
pub async fn file_modified_time_store_batch(
    store: Arc<dyn crate::store::MutableStore>,
    partition: RepositoryId,
    items: Vec<(Hash, u64)>,
) {
    for (key, mtime) in items {
        let _ = store
            .clone()
            .store(partition, key, Hash::from_u64(mtime), KeyType::Untyped)
            .await;
    }
}

pub async fn file_modified_time_clear(repository: Arc<RepositoryContext>, path: impl AsRef<str>) {
    let path = path.as_ref();
    lore_trace!("Clear mtime for {path}");
    let Some(handle) = repository.try_write_mutable_store() else {
        return;
    };
    let key = file_modified_time_key(repository.salt(), repository.instance_id, path);
    let _ = handle
        .store(repository.id, key, Hash::default(), KeyType::Untyped)
        .await;
}

pub async fn verify_node_name_case(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
) -> Result<(), StateError> {
    verify_node_name_case_impl(repository, state, node).await
}

fn verify_node_name_case_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send>> {
    Box::pin(verify_node_name_case_impl(repository, state, node))
}

async fn verify_node_name_case_impl(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
) -> Result<(), StateError> {
    let nodes = state
        .collect_children_unsorted(
            repository.clone(),
            node,
            false, /* Do not include deleted nodes */
            true,  /* Traverse links */
        )
        .await?;
    if nodes.children.is_empty() {
        return Ok(());
    }

    for index in 0..(nodes.children.len() - 1) {
        let current_named_node = &nodes.children[index];
        let current_block = nodes
            .state
            .block_with_nametable(
                nodes.repository.clone(),
                NodeBlock::index(current_named_node.node),
            )
            .await?;
        let current_node = current_block.node(Node::index(current_named_node.node));
        if current_node.is_staged_delete() || current_node.is_discarded() {
            continue;
        }

        let first_path = nodes
            .state
            .node_path(nodes.repository.clone(), current_named_node.node)
            .await?;

        lore_trace!(
            "Check node name case for siblings of {} {}",
            first_path,
            current_named_node.node
        );

        for next_named_node in nodes.children.iter().skip(index + 1) {
            let next_block = nodes
                .state
                .block_with_nametable(
                    nodes.repository.clone(),
                    NodeBlock::index(next_named_node.node),
                )
                .await?;
            let next_node = next_block.node(Node::index(next_named_node.node));
            if next_node.is_staged_delete() || next_node.is_discarded() {
                continue;
            }

            //let next_name = state.node_name_direct(&next_node, &next_nametable);
            //let next_hash = hash_string_lowercase(next_name);

            if current_node.name_hash != next_node.name_hash {
                continue;
            }

            let second_path = nodes
                .state
                .node_path(nodes.repository.clone(), next_named_node.node)
                .await?;

            // TODO(mjansson): User input should be behind an --interactive command line/API option
            //                 or a flag to select automatic resolve behaviour. Revisit this once
            //                 structured output is in place. Potentially remove this healing code
            //                 path once name case variations cannot be created anymore.
            let selection = if current_named_node.name != next_named_node.name {
                println!(
                    "Node differ only by case:\n1) {} (node {})\n2) {} (node {})",
                    first_path, current_named_node.node, second_path, next_named_node.node
                );
                print!("Select which name to use (1 or 2, or anything else to abort)> ");
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);
                input.trim().to_string()
            } else {
                let option = if current_named_node.node < next_named_node.node {
                    "1".to_string()
                } else {
                    "2".to_string()
                };
                lore_warn!(
                    "Multiple nodes have identical name, unifying by selecting option {option}:\n1) {} (node {})\n2) {} (node {})",
                    first_path,
                    current_named_node.node,
                    second_path,
                    next_named_node.node
                );
                option
            };

            let mut keep_node = current_named_node;
            let mut delete_node = next_named_node;
            match selection.as_str() {
                "1" => {
                    // Use the already setup node combo
                }
                "2" => {
                    keep_node = next_named_node;
                    delete_node = current_named_node;
                }
                _ => {
                    println!("No option selected, aborting");
                    return Err(StateError::internal("Name case clash"));
                }
            }

            lore_trace!(
                "Keep {} node {:?}, delete node {:?}",
                second_path,
                keep_node,
                delete_node
            );

            stage_delete(
                nodes.repository.clone(),
                nodes.state.clone(),
                delete_node.node,
                NodeFlags::NoFlags,
                Arc::default(),
                None, // No link tracking in state verification
            )
            .await
            .internal("Verify delete")?;

            if delete_node.node == current_named_node.node {
                break;
            }
        }
    }

    for named_node in nodes.children.iter() {
        let node = nodes
            .state
            .node(nodes.repository.clone(), named_node.node)
            .await?;
        if node.is_directory() && !node.is_staged_delete() {
            lore_trace!(
                "Recurse check node name case for children of {}",
                named_node.node
            );
            verify_node_name_case_recurse(
                nodes.repository.clone(),
                nodes.state.clone(),
                named_node.node,
            )
            .await?;
        }
    }

    Ok(())
}

async fn collect_state_fragments(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<Vec<Address>, StateError> {
    let mut addresses = Vec::with_capacity(32);

    {
        let data = state.data.read();
        addresses.push(Address::zero_context_hash(data.hash_link));
        addresses.push(Address::zero_context_hash(data.hash_metadata));
        addresses.push(Address::zero_context_hash(data.hash_tree));
    }

    let tree = state.tree(repository.clone()).await?;
    addresses.push(Address::zero_context_hash(tree.hash_node));
    addresses.push(Address::zero_context_hash(tree.hash_file_metadata));
    addresses.push(Address::zero_context_hash(tree.hash_delta));

    if !tree.hash_nametable_deprecated.is_zero() {
        addresses.push(Address::zero_context_hash(tree.hash_nametable_deprecated));
    }

    addresses.push(Address::zero_context_hash(state.revision()));

    Ok(addresses)
}

async fn collect_node_blocks(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<Vec<Address>, StateError> {
    let mut addresses = Vec::with_capacity(32);

    let tree = state.tree(repository.clone()).await?;
    if !tree.hash_node.is_zero() {
        let block_address = Address::zero_context_hash(tree.hash_node);
        let buffer = immutable::read(
            repository.clone(),
            block_address,
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(&repository)
                .with_cache()
                .with_priority(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;

        let hash_slice = buffer.as_type_slice::<Hash>();
        addresses.reserve(hash_slice.len());
        for hash in hash_slice.iter() {
            addresses.push(Address::zero_context_hash(*hash));
        }
    }

    Ok(addresses)
}

async fn collect_file_metadata_blocks(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<Vec<Address>, StateError> {
    let mut addresses = Vec::with_capacity(32);

    let tree = state.tree(repository.clone()).await?;
    if !tree.hash_file_metadata.is_zero() {
        let block_address = Address::zero_context_hash(tree.hash_file_metadata);
        let buffer = immutable::read(
            repository.clone(),
            block_address,
            None, /* Read the full array of block hashes */
            immutable::read_options_from_repository(&repository)
                .with_cache()
                .with_priority(),
        )
        .await
        .forward::<StateError>("Failed to deserialize node block list")?;

        let hash_slice = buffer.as_type_slice::<Hash>();
        addresses.reserve(hash_slice.len());
        for hash in hash_slice.iter() {
            addresses.push(Address::zero_context_hash(*hash));
        }
    }

    Ok(addresses)
}

async fn collect_name_fragments(
    repository: Arc<RepositoryContext>,
    blocks: Vec<Address>,
) -> Result<Vec<Address>, StateError> {
    let mut tasks = JoinSet::new();
    for address in blocks {
        lore_spawn!(tasks, {
            let repository = repository.clone();
            async move {
                if let Ok(block_data) =
                    NodeBlockData::read_box_from_immutable(repository.clone(), address, true).await
                {
                    Ok(block_data.name_table)
                } else {
                    // Make sure block can be read even though it has no local name table
                    let _block_data =
                        NodeBlockDataV0::read_box_from_immutable(repository.clone(), address, true)
                            .await
                            .internal("Failed to deserialize node block")?;
                    Ok(Hash::default())
                }
            }
        });
    }
    let mut name = Vec::with_capacity(tasks.len());
    let mut failure = None;
    while let Some(result) = tasks.join_next().await {
        match result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()
        {
            Ok(hash) => {
                if !hash.is_zero() {
                    name.push(Address::zero_context_hash(hash));
                }
            }
            Err(err) => {
                failure = failure.or(Some(err));
            }
        }
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(name)
}

fn collect_diff_addresses(from: Vec<Address>, to: Vec<Address>) -> Vec<Address> {
    let mut new = Vec::with_capacity(to.len());
    let mut ifrom = 0;
    let mut ito = 0;
    while ifrom < from.len() && ito < to.len() {
        match from[ifrom].cmp(&to[ito]) {
            std::cmp::Ordering::Less => {
                ifrom += 1;
            }
            std::cmp::Ordering::Greater => {
                new.push(to[ito]);
                ito += 1;
            }
            std::cmp::Ordering::Equal => {
                ifrom += 1;
                ito += 1;
            }
        }
    }
    new.extend_from_slice(&to[ito..]);
    new
}

pub async fn collect_new_fragments(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    ignore_durably_stored: bool,
) -> Result<Vec<Address>, StateError> {
    let from_state_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_from.clone();
        async move {
            let addresses = collect_state_fragments(repository.clone(), state).await?;
            // Collect all from block addresses, even uploaded, as we want to diff against these
            let mut addresses = collect_new_addresses(repository, &addresses, false).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let to_new_state_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_to.clone();
        async move {
            let addresses = collect_state_fragments(repository.clone(), state).await?;
            let mut addresses =
                collect_new_addresses(repository, &addresses, ignore_durably_stored).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let from_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_from.clone();
        async move {
            // Blocks are never fragmented, safe to not call collect_new_addresses
            let mut addresses = collect_node_blocks(repository, state).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let to_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_to.clone();
        async move {
            // Blocks are never fragmented, safe to not call collect_new_addresses
            let mut addresses = collect_node_blocks(repository, state).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let from_file_metadata_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_from.clone();
        async move {
            // Collect all from metadata block addresses, even uploaded, as we want to diff against these
            let addresses = collect_file_metadata_blocks(repository, state).await?;
            Ok(addresses)
        }
    });

    let to_file_metadata_block_address = lore_spawn!({
        let repository = repository.clone();
        let state = state_to.clone();
        async move {
            // Collect all to metadata block addresses, even uploaded, as we want to inspect and load these
            let addresses = collect_file_metadata_blocks(repository, state).await?;
            Ok(addresses)
        }
    });

    let new_file_address = lore_spawn!({
        let repository = repository.clone();
        let state_from = state_from.clone();
        let state_to = state_to.clone();
        async move {
            // Safe to filter these directly to only contain not uploaded fragments, we don't
            // use it as input to any other collection
            collect_new_file_fragments(
                repository,
                state_from,
                state_to,
                ROOT_NODE,
                ROOT_NODE,
                ignore_durably_stored,
            )
            .await
        }
    });

    let mut failure = None;

    // Get the diff of the node blocks addresses
    let from_block_address = from_block_address.await;
    let to_block_address = to_block_address.await;
    let from_block_address = match from_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let to_block_address = match to_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let from_len = from_block_address.len();
    let to_len = to_block_address.len();
    let diff_block_address = if failure.is_none() {
        collect_diff_addresses(from_block_address, to_block_address)
    } else {
        vec![]
    };
    let diff_block_address_len = diff_block_address.len();
    lore_debug!(
        "Collecting fragments, from blocks {from_len}, to blocks {to_len} -> {diff_block_address_len} diff",
    );

    // Get the new name tables for the new node blocks
    let new_name_address = lore_spawn!({
        let repository = repository.clone();
        let blocks = diff_block_address.clone();
        async move {
            let addresses = collect_name_fragments(repository.clone(), blocks).await?;
            let mut addresses =
                collect_new_addresses(repository, &addresses, ignore_durably_stored).await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    // Get the actual new block addresses, ignore already uploaded now that we have
    // collected the name block addresses from the diff list
    let new_block_address = lore_spawn!({
        let repository = repository.clone();
        async move {
            let mut addresses =
                collect_new_addresses(repository, &diff_block_address, ignore_durably_stored)
                    .await?;
            addresses.sort_unstable();
            Ok(addresses)
        }
    });

    let from_state_address = from_state_address.await;
    let to_new_state_address = to_new_state_address.await;
    let from_state_address = match from_state_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let to_new_state_address = match to_new_state_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let from_len = from_state_address.len();
    let to_len = to_new_state_address.len();
    let mut new_state_address = if failure.is_none() {
        collect_diff_addresses(from_state_address, to_new_state_address)
    } else {
        vec![]
    };
    lore_debug!(
        "Collecting fragments, from state {from_len}, to state new {to_len} -> {} new",
        new_state_address.len()
    );

    let from_file_metadata_block_address = from_file_metadata_block_address.await;
    let to_file_metadata_block_address = to_file_metadata_block_address.await;
    let from_file_metadata_block_address = match from_file_metadata_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    let to_file_metadata_block_address = match to_file_metadata_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };

    let from_file_metadata_block_len = from_file_metadata_block_address.len();
    let to_file_metadata_block_len = to_file_metadata_block_address.len();
    let (mut new_file_metadata_block_address, mut new_file_metadata_blob_address_tasks) = if failure
        .is_none()
    {
        let mut new_file_metadata_address = vec![];
        let mut tasks = JoinSet::new();
        for (block_index, to_block_address) in to_file_metadata_block_address.iter().enumerate() {
            if to_block_address.hash.is_zero() {
                continue;
            }

            let from_block_address = from_file_metadata_block_address.get(block_index).cloned();
            if from_block_address != Some(*to_block_address) {
                new_file_metadata_address.push(*to_block_address);

                // Check which metadata is new
                let repository = repository.clone();
                let state_to = state_to.clone();
                lore_spawn!(
                    tasks,
                    collect_new_node_metadata_fragments(
                        repository,
                        state_to,
                        from_block_address,
                        *to_block_address,
                        block_index,
                        ignore_durably_stored,
                    )
                );
            }
        }
        (new_file_metadata_address, tasks)
    } else {
        (vec![], JoinSet::new())
    };
    lore_debug!(
        "Collecting fragments, from file metadata {from_file_metadata_block_len} blocks, to file metadata new {to_file_metadata_block_len} blocks -> {} new blocks",
        new_file_metadata_block_address.len()
    );

    let new_block_address = new_block_address.await;
    let new_name_address = new_name_address.await;

    let mut new_block_address = match new_block_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected node block from {} diff -> {} new",
        diff_block_address_len,
        new_block_address.len(),
    );
    let mut new_name_address = match new_name_address
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected name block from {} diff -> {} new",
        diff_block_address_len,
        new_name_address.len(),
    );

    let mut new_file_metadata_blob_address = vec![];
    while let Some(result) = new_file_metadata_blob_address_tasks.join_next().await {
        match result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()
        {
            Ok(mut address) => new_file_metadata_blob_address.append(&mut address),
            Err(err) => failure = failure.or(Some(err)),
        }
    }
    lore_debug!(
        "Collected file metadata blobs from {} blocks -> {} new",
        to_file_metadata_block_len,
        new_file_metadata_blob_address.len(),
    );

    let mut fragments = Vec::with_capacity(
        new_state_address.len()
            + new_block_address.len()
            + new_name_address.len()
            + new_file_metadata_block_address.len()
            + new_file_metadata_blob_address.len(),
    );

    fragments.append(&mut new_state_address);
    fragments.append(&mut new_block_address);
    fragments.append(&mut new_name_address);
    fragments.append(&mut new_file_metadata_block_address);
    fragments.append(&mut new_file_metadata_blob_address);

    lore_debug!("Collected {} new addresses from state", fragments.len());

    // Add branch metadata
    if let Ok((_current_revision, current_branch)) =
        crate::instance::load_current_anchor(&repository).await
        && let Ok(metadata_hash) = branch::metadata_hash(repository.clone(), current_branch).await
    {
        let metadata_fragments = collect_new_addresses(
            repository.clone(),
            &[Address::zero_context_hash(metadata_hash)],
            ignore_durably_stored,
        )
        .await;
        if let Ok(mut metadata_fragments) = metadata_fragments {
            lore_trace!(
                "Collected {} new addresses from branch metadata",
                fragments.len()
            );
            fragments.append(&mut metadata_fragments);
        } else {
            failure = failure.or(metadata_fragments.err());
        }
    }

    // Collect new file node fragments
    let mut new_file_address = match new_file_address
        .await
        .internal("Task failure")
        .map_err(StateError::from)
        .flatten()
    {
        Ok(address) => address,
        Err(err) => {
            failure = failure.or(Some(err));
            vec![]
        }
    };
    lore_debug!(
        "Collected {} new addresses from files",
        new_file_address.len()
    );

    if let Some(err) = failure {
        return Err(err);
    }

    fragments.append(&mut new_file_address);

    fragments.sort_unstable();
    fragments.dedup();

    Ok(fragments)
}

async fn collect_new_file_fragments(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    node_from: NodeID,
    node_to: NodeID,
    ignore_durably_stored: bool,
) -> Result<Vec<Address>, StateError> {
    let (from, to) = join!(
        state_from.collect_children_unsorted(
            repository.clone(),
            node_from,
            false, /* No deleted */
            false, /* No links, pushed separately */
        ),
        state_to.collect_children_unsorted(
            repository.clone(),
            node_to,
            false, /* No deleted */
            false, /* No links, pushed separately */
        )
    );
    let from = from?;
    let to = to?;

    let mut tasks = JoinSet::new();
    let mut failure = None;
    for to_named_node in to.children {
        let to_node_id = to_named_node.node;
        let to_node = to.state.node(to.repository.clone(), to_node_id).await;
        let Ok(to_node) = to_node else {
            failure = failure.or(to_node.err());
            break;
        };
        let mut from_node_id = INVALID_NODE;
        let mut modified = false;
        for from_named_node in from.children.iter() {
            if from_named_node.name == to_named_node.name {
                let from_node = from
                    .state
                    .node(from.repository.clone(), from_named_node.node)
                    .await?;
                from_node_id = from_named_node.node;
                if to_node.address != from_node.address {
                    modified = true;
                }
                break;
            }
        }

        if !from_node_id.is_valid_node_id() || modified {
            if to_node.is_file() {
                let repository = to.repository.clone();
                let address = [to_node.address];
                lore_spawn!(tasks, async move {
                    collect_new_addresses(repository, &address, ignore_durably_stored).await
                });
            } else {
                let repository = to.repository.clone();
                let state_from = from.state.clone();
                let state_to = to.state.clone();
                lore_spawn!(tasks, async move {
                    collect_new_file_fragments_recurse(
                        repository,
                        state_from,
                        state_to,
                        from_node_id,
                        to_node_id,
                        ignore_durably_stored,
                    )
                    .await
                });
            }
        }
    }

    let mut new_addresses = vec![];
    while let Some(result) = tasks.join_next().await {
        match result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()
        {
            Ok(mut address) => {
                new_addresses.append(&mut address);
            }
            Err(err) => {
                failure = failure.or(Some(err));
            }
        }
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(new_addresses)
}

fn collect_new_file_fragments_recurse(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    node_from: NodeID,
    node_to: NodeID,
    ignore_durably_stored: bool,
) -> Pin<Box<dyn Future<Output = Result<Vec<Address>, StateError>> + Send + 'static>> {
    Box::pin(collect_new_file_fragments(
        repository,
        state_from,
        state_to,
        node_from,
        node_to,
        ignore_durably_stored,
    ))
}

async fn collect_new_node_metadata_fragments(
    repository: Arc<RepositoryContext>,
    state_to: Arc<State>,
    block_address_from: Option<Address>,
    block_address_to: Address,
    block_index: usize,
    ignore_durably_stored: bool,
) -> Result<Vec<Address>, StateError> {
    let metadata_block_from = if let Some(address) = block_address_from {
        NodeFileMetadataBlockData::read_box_from_immutable_compat(repository.clone(), address, true)
            .await
            .internal("Failed to deserialize metadata")?
    } else {
        NodeFileMetadataBlockData::new_from_heap_zeroed()
    };

    let metadata_block_to = NodeFileMetadataBlockData::read_box_from_immutable_compat(
        repository.clone(),
        block_address_to,
        true,
    )
    .await
    .internal("Failed to deserialize metadata")?;

    let mut metadata_blobs = vec![];
    {
        let node_block_to = state_to.block(repository.clone(), block_index).await?;
        let node_block_to = node_block_to.read();

        for node_index in 0..metadata_block_to.node.len() {
            let metadata_hash = metadata_block_to.node[node_index].metadata;
            if metadata_block_from.node[node_index].metadata == metadata_hash
                || metadata_hash.is_zero()
            {
                continue;
            }

            // We need to check if the node is actually in use or old stale data
            if node_block_to.is_node_in_use(node_index) {
                metadata_blobs.push(Address::zero_context_hash(metadata_hash));
            }
        }
    }

    let mut metadata_refs = vec![];
    let mut addresses_expected = 0;
    for metadata_blob in metadata_blobs.iter() {
        let metadata = Metadata::deserialize(repository.clone(), metadata_blob.hash)
            .await
            .internal("Failed to deserialize metadata")?;

        metadata
            .walk(
                |_key_slice: &[u8], value_slice: &[u8], value_type: MetadataType| {
                    if value_type == MetadataType::Address {
                        if let Ok(address) = Metadata::to_address(value_slice) {
                            if address.hash.is_zero() {
                                return;
                            }
                            metadata_refs.push(address);
                        }
                        addresses_expected += 1;
                    }
                },
            )
            .internal("Failed to deserialize metadata")?;
    }

    // Ensure metadata contained only valid addresses
    if addresses_expected != metadata_refs.len() {
        return Err(StateError::internal("Invalid metadata address"));
    }

    let mut addresses =
        collect_new_addresses(repository.clone(), &metadata_blobs, ignore_durably_stored).await?;
    let mut more_addresses =
        collect_new_addresses(repository, &metadata_refs, ignore_durably_stored).await?;
    addresses.append(&mut more_addresses);

    addresses.sort_unstable();
    addresses.dedup();

    Ok(addresses)
}

async fn collect_new_addresses(
    repository: Arc<RepositoryContext>,
    addresses: &[Address],
    ignore_durably_stored: bool,
) -> Result<Vec<Address>, StateError> {
    let mut new_addresses = Vec::with_capacity(addresses.len());

    const MAX_TASKS: usize = 1000;
    let mut task = JoinSet::new();
    for address in addresses {
        if address.hash.is_zero() {
            continue;
        }

        let address = *address;
        let repository = repository.clone();
        lore_spawn!(task, {
            async move {
                if let Ok(query) = repository
                    .immutable_store()
                    .query(repository.id, address, StoreMatch::MatchFull)
                    .await
                {
                    let mut addresses = vec![];
                    if query.fragment.flags & FragmentFlags::PayloadFragmented != 0
                        && let Ok((_fragment, buffer)) = immutable::load_raw(
                            repository.clone(),
                            address,
                            immutable::read_options_from_repository(&repository),
                        )
                        .await
                    {
                        let buffer = buffer.to_aligned::<FragmentReference>();
                        let mut subaddress =
                            Vec::with_capacity(buffer.count::<FragmentReference>());
                        for reference in buffer.as_type_slice::<FragmentReference>().iter() {
                            subaddress.push(Address {
                                context: address.context,
                                hash: reference.hash,
                            });
                        }
                        if let Ok(mut subaddress) = collect_new_addresses_recurse(
                            repository.clone(),
                            subaddress.as_slice(),
                            ignore_durably_stored,
                        )
                        .await
                        {
                            addresses.append(&mut subaddress);
                        }
                    }

                    if !ignore_durably_stored
                        || query.match_made != StoreMatch::MatchFull
                        || (query.fragment.flags & FragmentFlags::PayloadStoredDurable) == 0
                    {
                        addresses.push(address);
                    }

                    if !addresses.is_empty() {
                        Some(addresses)
                    } else {
                        None
                    }
                } else {
                    Some(vec![address])
                }
            }
        });

        while task.len() > MAX_TASKS {
            if let Some(result) = task.join_next().await
                && let Some(mut address) = result.internal("Task failure")?
            {
                new_addresses.append(&mut address);
            }
        }
    }

    while let Some(result) = task.join_next().await {
        if let Some(mut address) = result.internal("Task failure")? {
            new_addresses.append(&mut address);
        }
    }

    Ok(new_addresses)
}

fn collect_new_addresses_recurse(
    repository: Arc<RepositoryContext>,
    addresses: &[Address],
    ignore_durably_stored: bool,
) -> Pin<Box<dyn Future<Output = Result<Vec<Address>, StateError>> + Send + '_>> {
    Box::pin(collect_new_addresses(
        repository,
        addresses,
        ignore_durably_stored,
    ))
}

/// Applies a set of node-level changes to a state tree without touching the filesystem.
///
/// This is used for server-side merge operations where there is no working directory.
/// Changes are applied purely at the state tree level: nodes are added, modified, or
/// deleted in the target state based on the diff result.
///
/// The `target_state` is the state being modified (e.g., the current branch head).
/// Each `NodeChange` carries source node data in its `to` field, which is copied into
/// the target state tree.
///
/// Preconditions:
/// - The changes must be conflict-free (no entries with `Flags::Conflict`)
/// - All fragment data referenced by the changes must already exist in the immutable store
pub async fn apply_tree_changes(
    repository: Arc<RepositoryContext>,
    target_state: Arc<State>,
    changes: &[NodeChange],
) -> Result<(), StateError> {
    let stats = Arc::new(crate::stage::StageStats::default());

    // Process deletes first, in reverse path order (deepest paths first) so that
    // children are deleted before parent directories
    let mut delete_changes: Vec<&NodeChange> = changes
        .iter()
        .filter(|c| c.action == FileAction::Delete)
        .collect();
    delete_changes.sort_by_key(|b| std::cmp::Reverse(b.path.as_str().len()));

    for change in &delete_changes {
        let node_link = match target_state
            .find_node_link(repository.clone(), change.path.as_str())
            .await
        {
            Ok(node_link) => node_link,
            Err(e) if e.is_node_not_found() => continue,
            Err(err) => return Err(err),
        };

        if node_link.is_valid() {
            crate::stage::stage_delete(
                repository.clone(),
                target_state.clone(),
                node_link.node,
                NodeFlags::StagedMerge,
                stats.clone(),
                None,
            )
            .await
            .internal("Node not found")?;
        }
    }

    // Process add/modify/move changes
    for change in changes {
        if change.action == FileAction::Delete {
            continue;
        }

        // For move actions, delete the old path first
        if change.action == FileAction::Move
            && let Some(from_path) = change.from_path.as_ref()
        {
            let node_link = match target_state
                .find_node_link(repository.clone(), from_path.as_str())
                .await
            {
                Ok(node_link) => node_link,
                Err(e) if e.is_node_not_found() => NodeLink::invalid(),
                Err(err) => return Err(err),
            };

            if node_link.is_valid() {
                crate::stage::stage_delete(
                    repository.clone(),
                    target_state.clone(),
                    node_link.node,
                    NodeFlags::StagedMerge,
                    stats.clone(),
                    None,
                )
                .await
                .internal("Node not found")?;
            }
        }

        // Get the source node data from the change
        let source_state = &change.to.state;
        let source_node_id = change.to.node;
        if !source_node_id.is_valid_node_id() {
            continue;
        }

        let node = source_state
            .node(change.to.repository.clone(), source_node_id)
            .await?;

        // Stage the node into the target state at the change path
        crate::stage::stage_single_node(
            repository.clone(),
            target_state.clone(),
            change.path.clone(),
            node,
            stats.clone(),
            None,
            crate::filter::FilterMode::Full,
        )
        .await
        .internal("Node not found")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_branch_returns_parent_when_branch_is_zero() {
        let link_ref = LinkReference {
            branch: BranchId::default(),
            ..LinkReference::default()
        };
        let parent = BranchId::from([1u8; 16]);
        assert_eq!(link_ref.resolve_branch(parent), parent);
    }

    #[test]
    fn resolve_branch_returns_own_branch_when_non_zero() {
        let own_branch = BranchId::from([2u8; 16]);
        let link_ref = LinkReference {
            branch: own_branch,
            ..LinkReference::default()
        };
        let parent = BranchId::from([1u8; 16]);
        assert_eq!(link_ref.resolve_branch(parent), own_branch);
    }
}
