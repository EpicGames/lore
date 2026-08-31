// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::ProtocolError;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use super::RepositoryContext;
use super::RepositoryError;
use crate::event;
use crate::hash;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_trace;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::state;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::store::StoreMatch;
use crate::util::path::RelativePath;

/// Data for the event emitted when state verification starts.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryVerifyStateBeginEventData {
    /// Placeholder field. The event carries no data.
    pub _unused: u32,
}

/// Data for the event emitted when state verification finishes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryVerifyStateEndEventData {
    /// Identifier of the staged state after healing. Zero when nothing was healed.
    pub healed_staged_state: Hash,
}

/// One stored copy of a fragment found during fragment verification.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryVerifyFragmentMatchEventData {
    /// Slot the match was found in.
    pub slot: u32,
    /// Index of the match within the slot.
    pub index: u32,
    /// Identifier of the repository the match belongs to.
    pub repository: RepositoryId,
    /// Hash part of the fragment address.
    pub address_hash: Hash,
    /// Context part of the fragment address.
    pub address_context: Context,
    /// Storage flags recorded for the fragment.
    pub flags: u32,
    /// Stored size of the fragment payload in bytes.
    pub size_payload: u32,
    /// Size of the fragment content in bytes.
    pub size_content: u64,
    /// Offset of the fragment within its pack file.
    pub pack_offset: u32,
    /// Index of the pack file holding the fragment.
    pub pack_file: u32,
    /// Time the fragment was last accessed, in seconds since the Unix epoch.
    pub last_access: u64,
}

/// Result of verifying a single fragment, including every stored copy found.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryVerifyFragmentEventData {
    /// Hash of the fragment that was verified.
    pub hash: Hash,
    /// Index of the group the fragment belongs to.
    pub group_index: u32,
    /// Index of the bucket the fragment belongs to.
    pub bucket_index: u32,
    /// Path of the index file examined for the fragment.
    pub index_path: LoreString,
    /// Number of entries in the index.
    pub entry_count: u32,
    /// Number of entries in the pack file.
    pub packfile_entry_count: u32,
    /// Number of stored copies found for the fragment.
    pub match_count: u32,
    /// The stored copies found for the fragment.
    pub matches: LoreArray<LoreRepositoryVerifyFragmentMatchEventData>,
    /// Error message produced during verification. Empty on success.
    pub error: LoreString,
}

/// Result of verifying a single fragment on the remote.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryVerifyFragmentRemoteEventData {
    /// Hash part of the fragment address.
    pub address_hash: Hash,
    /// Context part of the fragment address.
    pub address_context: Context,
    /// Non-zero when the fragment was found to be corrupted.
    pub corrupted: u8,
    /// Non-zero when the fragment was healed.
    pub healed: u8,
    /// Error message produced during verification. Empty on success.
    pub error: LoreString,
}

/// Arguments for `verify_fragment` from the library layer
pub struct VerifyFragmentArgs {
    pub hash: LoreString,
    pub context: LoreString,
    pub heal: bool,
}

/// Validate a staged in-memory state on its way to becoming a revision.
///
/// Reads only through the state's own blocks — no working tree, no instance
/// anchors, no healing, nothing prompted. Rejects the tree shapes the push-side
/// validator rejects and a programmatic caller can build where the working tree
/// cannot: a child whose parent link does not lead back to its parent, a looped
/// sibling chain, a name whose hash does not match the stored string, and a staged
/// name a live sibling already holds.
///
/// Walks only what the commit will freeze — the same shape [`commit_directory`]
/// and [`rehash_directory`] walk, descending into a directory only when it is
/// staged. `node_mark` propagates the plain `Staged` bit to the root, so every
/// directory holding a change is reached and a settled subtree is never read. The
/// consequence is that two *settled* siblings sharing a name are not caught; they
/// come from a revision that was validated when it was published.
///
/// Staged and dirty flags are otherwise not examined. They are still set when a
/// commit calls this, and clearing them is the freeze's job — `rehash_directory`
/// is what refuses a leftover.
///
/// [`commit_directory`]: crate::commit
/// [`rehash_directory`]: crate::commit::rehash_directory
pub async fn verify_state_for_commit(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<(), RepositoryError> {
    verify_staged_directory(repository, state, ROOT_NODE).await
}

/// A live child of a directory the walk is validating.
struct ChildName {
    node_id: NodeID,
    name_hash: u64,
    staged: bool,
    directory: bool,
}

/// Validate the staged children of one directory, then recurse into the staged
/// directories among them.
///
/// Sequential, like the freeze walk it precedes: the work is now proportional to
/// what changed rather than to the tree, so there is nothing to fan out.
async fn verify_staged_directory(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> Result<(), RepositoryError> {
    let parent = state
        .node(repository.clone(), node_id)
        .await
        .forward::<RepositoryError>("Failed to deserialize repository state")?;

    let mut names: Vec<ChildName> = Vec::new();
    let mut any_staged = false;
    let mut children =
        StateNodeChildrenIterator::from_parent(state.clone(), repository.clone(), node_id, &parent)
            .await
            .forward::<RepositoryError>("Invalid node hierarchy in revision state")?;

    while let Some((child_node_id, child_node)) = children
        .next()
        .await
        .forward::<RepositoryError>("Invalid node hierarchy in revision state")?
    {
        if child_node.is_staged_delete() || child_node.is_discarded() {
            continue;
        }
        let staged = child_node.is_staged();
        if staged {
            any_staged = true;
            verify_node_name_hash(
                repository.clone(),
                state.clone(),
                child_node_id,
                child_node.name_hash,
            )
            .await?;
        }
        names.push(ChildName {
            node_id: child_node_id,
            name_hash: child_node.name_hash,
            staged,
            directory: child_node.is_directory(),
        });
    }

    if any_staged {
        verify_no_sibling_claims_a_staged_name(repository.clone(), state.clone(), &mut names)
            .await?;
    }

    for child in names.iter().filter(|child| child.staged && child.directory) {
        verify_staged_directory_recurse(repository.clone(), state.clone(), child.node_id).await?;
    }

    Ok(())
}

/// Reject a live sibling holding a name this commit publishes.
///
/// Sorting the chain's live children by name hash puts every namesake next to its
/// twin, so one comparison per neighbouring pair settles the directory — against a
/// scan of the staged names for every sibling, which is the product of the two.
///
/// A pair of *settled* namesakes is left alone: it came from a revision that was
/// validated when it was published, and this commit does not republish it.
async fn verify_no_sibling_claims_a_staged_name(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    names: &mut [ChildName],
) -> Result<(), RepositoryError> {
    names.sort_unstable_by_key(|child| child.name_hash);
    for pair in names.windows(2) {
        let [first, second] = pair else { continue };
        if first.name_hash != second.name_hash || !(first.staged || second.staged) {
            continue;
        }
        let node_path = state
            .node_path(repository.clone(), second.node_id)
            .await
            .unwrap_or_default();
        return Err(RepositoryError::internal(format!(
            "Repository verification failed: Node {} shares a name with node {}: {node_path}",
            second.node_id, first.node_id
        )));
    }
    Ok(())
}

/// Reject a node whose stored name does not hash to the `name_hash` beside it.
async fn verify_node_name_hash(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    name_hash: u64,
) -> Result<(), RepositoryError> {
    let block = state
        .block_with_nametable(repository, NodeBlock::index(node_id))
        .await
        .forward::<RepositoryError>("Failed to deserialize repository state")?;
    let node_name = block
        .node_name_ref(Node::index(node_id))
        .forward::<RepositoryError>("Failed to get node name")?;
    let expected = hash::hash_string(&node_name);
    if name_hash != expected {
        return Err(RepositoryError::internal(format!(
            "Repository verification failed: Node {node_id} has invalid name hash for name: {node_name} hash is {expected:x} found {name_hash:x}"
        )));
    }
    Ok(())
}

fn verify_staged_directory_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> Pin<Box<dyn Future<Output = Result<(), RepositoryError>> + Send>> {
    Box::pin(verify_staged_directory(repository, state, node_id))
}

pub async fn verify(
    repository: Arc<RepositoryContext>,
    path: Option<RelativePath>,
    heal: bool,
) -> Result<(), RepositoryError> {
    event::LoreEvent::RepositoryVerifyStateBegin(LoreRepositoryVerifyStateBeginEventData::default())
        .send();

    lore_debug!("Verifying local immutable store");

    repository
        .immutable_store()
        .verify(heal)
        .await
        .forward::<RepositoryError>("Repository verification failed: Store failure")?;

    lore_debug!("Verifying anchored states");

    let (state_current, state, _branch) = State::deserialize_current_and_staged(repository.clone())
        .await
        .forward::<RepositoryError>("Failed to deserialize repository state")?;
    let is_staged = state.is_some();
    let state = state.unwrap_or_else(|| state_current.clone());

    //state.force_rehash_names().await;

    // Check the node hierarchy
    let node_id = if let Some(path) = path {
        let node_link = state
            .find_node_link(repository.clone(), path.as_str())
            .await
            .forward::<RepositoryError>("Invalid repository path")?;
        node_link.node
    } else {
        ROOT_NODE
    };

    if !node_id.is_valid_or_root_node_id() {
        return Err(RepositoryError::internal("Invalid repository path"));
    }

    verify_node(repository.clone(), state.clone(), is_staged, node_id).await?;

    // Check case uniqueness of each node
    state::verify_node_name_case(repository.clone(), state.clone(), node_id)
        .await
        .forward::<RepositoryError>("Nodes have same name with only case variations")?;

    if state.is_dirty()
        && let Some(token) = repository.try_write_token()
    {
        // Healed, serialize new staged state
        lore_info!("Serializing healed state as new staged state");

        state.set_parent_self(state_current.revision());
        state.set_revision_number(0);

        if !is_staged {
            // Force commit same revision, reset metadata
            state.set_parent_other(Hash::default());
            state.set_metadata_hash(Hash::default());
        }

        let signature = state
            .serialize(repository.clone(), token)
            .await
            .forward::<RepositoryError>("Failed to serialize new healed staged state")?;
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<RepositoryError>("Failed to serialize repository anchor")?;

        event::LoreEvent::RepositoryVerifyStateEnd(LoreRepositoryVerifyStateEndEventData {
            healed_staged_state: signature,
        })
        .send();
    } else {
        event::LoreEvent::RepositoryVerifyStateEnd(LoreRepositoryVerifyStateEndEventData {
            healed_staged_state: Hash::default(),
        })
        .send();
    }

    Ok(())
}

async fn verify_node(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    is_staged: bool,
    node_id: NodeID,
) -> Result<(), RepositoryError> {
    let mut tasks = JoinSet::new();
    let mut result = verify_node_single(
        repository, state, is_staged, node_id, None, &mut tasks, None,
    )
    .await;

    while let Some(task_result) = tasks.join_next().await {
        if let Ok(task_result) = task_result {
            if result.is_ok() && task_result.is_err() {
                result = task_result;
            }
        } else {
            let subresult = Err(RepositoryError::internal(format!(
                "Repository verification failed: Task failure: {}",
                task_result.unwrap_err()
            )));
            if result.is_ok() {
                result = subresult;
            }
        };
    }

    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn verify_node_single(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    is_staged: bool,
    node_id: NodeID,
    expected_parent: Option<NodeID>,
    tasks: &mut JoinSet<Result<NodeID, RepositoryError>>,
    cycle: Option<&mut SiblingCycleGuard>,
) -> Result<NodeID, RepositoryError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await
        .forward::<RepositoryError>("Failed to deserialize repository state")?;
    let node = block.node(node_index);

    if let Some(expected_parent) = expected_parent {
        if let Some(cycle) = cycle {
            node.walk_step(node_id, expected_parent, cycle)
                .forward::<RepositoryError>("Invalid node hierarchy in revision state")?;
        } else {
            node.check_parent_link(node_id, expected_parent)
                .forward::<RepositoryError>("Invalid node hierarchy in revision state")?;
        }
    }

    lore_trace!(
        "Verify node {node_id} - name {} = {}",
        node.name_hash,
        state
            .node_name_ref(repository.clone(), node_id)
            .await
            .forward::<RepositoryError>("Failed to get node name")?
    );

    if !is_staged {
        if node.is_staged_delete()
            && let Ok(node_path) = state.node_path(repository.clone(), node_id).await
        {
            let node_flags = node.flags;
            return Err(RepositoryError::internal(format!(
                "Repository verification failed: Block {block_index} node {node_index} has deleted flag: {node_path} 0x{node_flags:x})"
            )));
        }

        if node.is_discarded() {
            if let Ok(node_path) = state.node_path(repository.clone(), node_id).await {
                let node_flags = node.flags;
                return Err(RepositoryError::internal(format!(
                    "Repository verification failed: Block {block_index} node {node_index} is discarded: {node_path} 0x{node_flags:x})"
                )));
            }
            // Discarded nodes have the next unused node in sibling pointer, abort
            return Ok(INVALID_NODE);
        }

        if node.is_staged()
            && let Ok(node_path) = state.node_path(repository.clone(), node_id).await
        {
            let node_flags = node.flags;
            return Err(RepositoryError::internal(format!(
                "Repository verification failed: Block {block_index} node {node_index} has staged flag: {node_path} 0x{node_flags:x})"
            )));
        }
    }

    if node_id != ROOT_NODE {
        let node_name = block
            .node_name_ref(node_index)
            .forward::<RepositoryError>("Failed to get node name")?;
        let name_hash = hash::hash_string(&node_name);
        if node.name_hash != name_hash {
            return Err(RepositoryError::internal(format!(
                "Repository verification failed: Block {block_index} node {node_index} has invalid name hash for name: {node_name} hash is {name_hash:x} found {:x}",
                node.name_hash
            )));
        }
    }

    if node.is_file() {
        // ...
    } else if node.is_link() {
        // TODO(vri): UCS-19231 - Links: Handle link nodes in repository verify
    } else if node.is_directory()
        && let Some(child) = node.child()
    {
        lore_spawn!(tasks, {
            let repository = repository.clone();
            let state = state.clone();
            async move {
                verify_node_recurse(repository.clone(), state.clone(), is_staged, child, node_id)
                    .await
            }
        });
    }

    Ok(node.sibling)
}

async fn verify_node_and_siblings(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    is_staged: bool,
    mut node_id: NodeID,
    expected_parent: NodeID,
) -> Result<NodeID, RepositoryError> {
    lore_trace!("Verify node {node_id} and siblings");

    let mut result: Result<NodeID, RepositoryError> = Ok(INVALID_NODE);
    let mut tasks = JoinSet::new();
    let mut cycle = SiblingCycleGuard::new(expected_parent);
    while node_id.is_valid_node_id() && result.is_ok() {
        result = verify_node_single(
            repository.clone(),
            state.clone(),
            is_staged,
            node_id,
            Some(expected_parent),
            &mut tasks,
            Some(&mut cycle),
        )
        .await;

        node_id = result.as_ref().copied().unwrap_or(INVALID_NODE);
    }

    while let Some(task_result) = tasks.join_next().await {
        if let Ok(task_result) = task_result {
            if result.is_ok() && task_result.is_err() {
                result = task_result;
            }
        } else {
            let subresult = Err(RepositoryError::internal(format!(
                "Repository verification failed: Task failure: {}",
                task_result.unwrap_err()
            )));
            if result.is_ok() {
                result = subresult;
            }
        };
    }

    result
}

fn verify_node_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    is_staged: bool,
    node: NodeID,
    expected_parent: NodeID,
) -> Pin<Box<dyn Future<Output = Result<NodeID, RepositoryError>> + Send>> {
    Box::pin(verify_node_and_siblings(
        repository,
        state,
        is_staged,
        node,
        expected_parent,
    ))
}

pub async fn verify_fragment(
    repository: Arc<RepositoryContext>,
    args: VerifyFragmentArgs,
) -> Result<(), RepositoryError> {
    // Parse the hash
    let hash = Hash::from_str(args.hash.as_str()).internal("Invalid address")?;

    // Parse optional context
    let context = if !args.context.is_empty() {
        Context::from_str(args.context.as_str()).internal("Invalid address")?
    } else {
        Context::default()
    };

    let address = Address { hash, context };

    if execution_context().globals().local() {
        verify_fragment_local(repository, address, args).await
    } else {
        verify_fragment_remote(repository, address, args.heal).await
    }
}

async fn verify_fragment_local(
    repository: Arc<RepositoryContext>,
    address: Address,
    args: VerifyFragmentArgs,
) -> Result<(), RepositoryError> {
    // Use current repository's ID
    let match_repository = repository.id;

    // Determine match level based on provided arguments
    let match_level = if !args.context.is_empty() {
        StoreMatch::MatchFull
    } else {
        StoreMatch::MatchHash
    };

    // Check if this is a local store
    let store = repository.immutable_store();
    if !store.is_local() {
        return Err(RepositoryError::internal(
            "Repository verification failed: verify_fragment requires a local store",
        ));
    }

    // Call verify_fragment on the concrete local store (downcast from trait object).
    // The `as_any` method on `ImmutableStore` requires `Sized` so we cannot call it
    // through a trait object. Instead, we go through the composite store which exposes
    // a `local()` accessor returning `Arc<dyn ImmutableStore>`. The local store is the
    // concrete `ImmutableStore` type, so we attempt a downcast via `as_any`.
    //
    // Since `ImmutableStore: Any`, the concrete type behind the `Arc<dyn ImmutableStore>`
    // carries its `TypeId`. We coerce the trait‐object pointer.
    let concrete_store: Arc<crate::store::immutable::ImmutableStore> = (store
        as Arc<dyn std::any::Any + Send + Sync>)
        .downcast::<crate::store::immutable::ImmutableStore>()
        .map_err(|_not_local| {
            RepositoryError::internal(
                "Repository verification failed: store is not a local ImmutableStore",
            )
        })?;

    let result = concrete_store
        .verify_fragment(address, match_repository, match_level, args.heal)
        .await
        .forward::<RepositoryError>("Repository verification failed: Failed to verify fragment")?;

    // Convert result to event data
    let matches: Vec<LoreRepositoryVerifyFragmentMatchEventData> = result
        .matches
        .iter()
        .map(|m| LoreRepositoryVerifyFragmentMatchEventData {
            slot: m.slot as u32,
            index: m.index as u32,
            repository: m.partition,
            address_hash: m.address.hash,
            address_context: m.address.context,
            flags: m.data.flags,
            size_payload: m.data.size_payload,
            size_content: m.data.size_content,
            pack_offset: m.data.pack_offset,
            pack_file: m.data.pack_file,
            last_access: m.data.last_access,
        })
        .collect();

    let error_string = match &result.verification_result {
        Ok(()) => String::new(),
        Err(e) => format!("{e}"),
    };

    event::LoreEvent::RepositoryVerifyFragment(LoreRepositoryVerifyFragmentEventData {
        hash: address.hash,
        group_index: result.group_index as u32,
        bucket_index: result.bucket_index as u32,
        index_path: LoreString::from(result.index_path.display().to_string()),
        entry_count: result.entry_count as u32,
        packfile_entry_count: result.packfile_entry_count as u32,
        match_count: matches.len() as u32,
        matches: LoreArray::from_vec(matches),
        error: LoreString::from(error_string),
    })
    .send();

    Ok(())
}

async fn verify_fragment_remote(
    repository: Arc<RepositoryContext>,
    address: Address,
    heal: bool,
) -> Result<(), RepositoryError> {
    let remote = repository
        .remote()
        .await
        .forward::<RepositoryError>("Repository verification failed: Store failure")?;

    let correlation_id = execution_context().globals().correlation_id.to_string();
    let storage = remote
        .session(repository.id, &correlation_id)
        .await
        .forward::<RepositoryError>("Repository verification failed: Failed to connect")?;

    let result = storage.verify(&address, heal).await;

    match result {
        Ok(verify_result) => {
            event::LoreEvent::RepositoryVerifyFragmentRemote(
                LoreRepositoryVerifyFragmentRemoteEventData {
                    address_hash: address.hash,
                    address_context: address.context,
                    corrupted: verify_result.corrupted as u8,
                    healed: verify_result.healed as u8,
                    error: LoreString::default(),
                },
            )
            .send();
            Ok(())
        }
        Err(e) => {
            let error_msg = match &e {
                ProtocolError::NotFound(_) => "Fragment not found".to_string(),
                _ => format!("Verification failed: {e}"),
            };
            event::LoreEvent::RepositoryVerifyFragmentRemote(
                LoreRepositoryVerifyFragmentRemoteEventData {
                    address_hash: address.hash,
                    address_context: address.context,
                    corrupted: 0,
                    healed: 0,
                    error: LoreString::from(error_msg.clone()),
                },
            )
            .send();
            Err(RepositoryError::internal(format!(
                "Repository verification failed: {error_msg}"
            )))
        }
    }
}
