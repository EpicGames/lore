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

use std::sync::Mutex;

use super::RepositoryContext;
use super::RepositoryError;
use crate::event;
use crate::fragment::FragmentFlags;
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

/// A file whose content could not be confirmed durable from the local store
/// alone: the local entry is missing or lacks `PayloadStoredDurable`. Resolved
/// against the remote store after the node walk.
struct DurabilitySuspect {
    address: Address,
    path: String,
}

type DurabilitySuspects = Arc<Mutex<Vec<DurabilitySuspect>>>;

pub async fn verify(
    repository: Arc<RepositoryContext>,
    path: Option<RelativePath>,
    heal: bool,
) -> Result<(), RepositoryError> {
    event::LoreEvent::RepositoryVerifyStateBegin(LoreRepositoryVerifyStateBeginEventData::default())
        .send();

    lore_debug!("Verifying local immutable store");

    repository.immutable_store().compact_stop().await;

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

    let suspects: DurabilitySuspects = Arc::new(Mutex::new(Vec::new()));
    verify_node(
        repository.clone(),
        state.clone(),
        is_staged,
        node_id,
        suspects.clone(),
    )
    .await?;

    // Resolve content whose durability could not be confirmed locally against
    // the remote store. Catches revisions published with content the remote
    // never received (e.g. a commit whose upload timed out).
    verify_content_durability(repository.clone(), suspects, heal).await?;

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
    suspects: DurabilitySuspects,
) -> Result<(), RepositoryError> {
    let mut tasks = JoinSet::new();
    let mut result = verify_node_single(
        repository, state, is_staged, node_id, None, &mut tasks, None, suspects,
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
    suspects: DurabilitySuspects,
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
        // Confirm the file's content is durable from the local store's
        // point of view; anything unconfirmed (missing entry, or an entry
        // whose upload never succeeded) is checked against the remote store
        // after the walk.
        if node.size > 0 && !node.address.hash.is_zero() {
            let query = repository
                .immutable_store()
                .query(repository.id, node.address, StoreMatch::MatchFull)
                .await;
            let durable_locally = matches!(
                &query,
                Ok(qr) if qr.match_made == StoreMatch::MatchFull
                    && qr.fragment.flags & FragmentFlags::PayloadStoredDurable != 0
            );
            if !durable_locally {
                let path = state
                    .node_path(repository.clone(), node_id)
                    .await
                    .map(|path| path.to_string())
                    .unwrap_or_default();
                suspects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(DurabilitySuspect {
                        address: node.address,
                        path,
                    });
            }
        }
    } else if node.is_link() {
        // TODO(vri): UCS-19231 - Links: Handle link nodes in repository verify
    } else if node.is_directory()
        && let Some(child) = node.child()
    {
        lore_spawn!(tasks, {
            let repository = repository.clone();
            let state = state.clone();
            let suspects = suspects.clone();
            async move {
                verify_node_recurse(
                    repository.clone(),
                    state.clone(),
                    is_staged,
                    child,
                    node_id,
                    suspects,
                )
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
    suspects: DurabilitySuspects,
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
            suspects.clone(),
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
    suspects: DurabilitySuspects,
) -> Pin<Box<dyn Future<Output = Result<NodeID, RepositoryError>> + Send>> {
    Box::pin(verify_node_and_siblings(
        repository,
        state,
        is_staged,
        node,
        expected_parent,
        suspects,
    ))
}

/// Check every durability suspect against the remote store: a file's content
/// that is not confirmed durable locally must exist on the remote, otherwise
/// the published revision references content other clients cannot fetch (fix
/// with `repository push-content` on the machine holding the payload).
///
/// Skipped when no remote is available (offline/local repository).
async fn verify_content_durability(
    repository: Arc<RepositoryContext>,
    suspects: DurabilitySuspects,
    heal: bool,
) -> Result<(), RepositoryError> {
    let suspects = std::mem::take(&mut *suspects.lock().unwrap_or_else(|e| e.into_inner()));
    if suspects.is_empty() {
        return Ok(());
    }

    let Ok(remote) = repository.remote().await else {
        lore_info!(
            "Skipping content durability check for {} file(s): no remote available",
            suspects.len()
        );
        return Ok(());
    };
    let correlation_id = execution_context().globals().correlation_id.to_string();
    let storage = remote
        .session(repository.id, &correlation_id)
        .await
        .forward::<RepositoryError>("Repository verification failed: Failed to connect")?;

    lore_debug!(
        "Checking {} file content address(es) against the remote store",
        suspects.len()
    );

    let mut missing = Vec::new();
    for suspect in &suspects {
        match storage.verify(&suspect.address, heal).await {
            Ok(_) => {}
            Err(err) => {
                let error_msg = match &err {
                    ProtocolError::NotFound(_) => "Fragment not found".to_string(),
                    _ => format!("Verification failed: {err}"),
                };
                event::LoreEvent::RepositoryVerifyFragmentRemote(
                    LoreRepositoryVerifyFragmentRemoteEventData {
                        address_hash: suspect.address.hash,
                        address_context: suspect.address.context,
                        corrupted: 0,
                        healed: 0,
                        error: LoreString::from(format!("{error_msg}: {}", suspect.path)),
                    },
                )
                .send();
                missing.push(suspect);
            }
        }
    }

    if let Some(first) = missing.first() {
        return Err(RepositoryError::internal(format!(
            "Repository verification failed: {} file(s) reference content missing from the remote store, e.g. {} ({}); run `repository push-content --scan` on the machine that committed them",
            missing.len(),
            first.path,
            first.address,
        )));
    }
    Ok(())
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
