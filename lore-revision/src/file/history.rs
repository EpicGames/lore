// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Serialize;

use crate::branch;
use crate::change::FileAction;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link::LinkedState;
use crate::lore::Address;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::metadata::Metadata;
use crate::node;
use crate::node::INVALID_NODE;
use crate::node::NodeDelta;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

/// Data for the event describing one entry in a file's history.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileHistoryEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision this entry belongs to.
    pub revision: Hash,
    /// Sequential number of the revision.
    pub revision_number: u64,
    /// Parent revisions of this revision.
    pub parent: [Hash; 2],
    /// Address of the file content at this revision.
    pub address: Address,
    /// Size of the file in bytes at this revision.
    pub size: u64,
    /// Action applied to the file at this revision.
    pub action: LoreFileAction,
    /// Path the file was moved from at this revision. Empty otherwise.
    pub from_path: LoreString,
}

impl LoreFileHistoryEventData {
    pub fn new(
        path: String,
        from_path: String,
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        address: Address,
        size: u64,
        action: u32,
    ) -> Self {
        let mut file_action = FileAction::Keep;
        if action == FileAction::Add as u32 {
            file_action = FileAction::Add;
        } else if action == FileAction::Delete as u32 {
            file_action = FileAction::Delete;
        } else if action == FileAction::Move as u32 {
            file_action = FileAction::Move;
        } else if action == FileAction::Copy as u32 {
            file_action = FileAction::Copy;
        }

        LoreFileHistoryEventData {
            path: path.into(),
            repository: repository.id,
            revision: state.revision(),
            revision_number: state.revision_number(),
            parent: [state.parent_self(), state.parent_other()],
            address,
            size,
            action: LoreFileAction::from(file_action),
            from_path: from_path.into(),
        }
    }
}

#[error_set]
pub enum FileHistoryError {
    Cancelled,
    LinkPinsDiverge,
    InvalidArguments,
    InvalidPath,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    PayloadNotFound,
    WriteRequired,
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

impl EventError for FileHistoryError {
    fn translated(&self) -> LoreError {
        match self {
            FileHistoryError::InvalidArguments(_) | FileHistoryError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            FileHistoryError::FileNotFound(_) => LoreError::FileNotFound,
            FileHistoryError::RevisionNotFound(_)
            | FileHistoryError::BranchNotFound(_)
            | FileHistoryError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct HistoryOptions {
    /// Optional revision specifier
    pub revision: Option<String>,

    /// Show revisions on specific branch
    pub branch: Option<String>,

    /// Number of revisions to list
    pub length: u32,

    /// Number of revisions to search initially
    pub depth: u32,
}

async fn node_path_in_revision(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    node_id: NodeID,
) -> String {
    if revision.is_zero() || node_id == INVALID_NODE {
        return String::new();
    }

    let state = match state::State::deserialize(repository.clone(), revision).await {
        Ok(state) => state,
        Err(err) => {
            lore_debug!("Failed to deserialize revision {revision} for previous path: {err}");
            return String::new();
        }
    };

    match state.node_path(repository, node_id).await {
        Ok(path) => path,
        Err(err) => {
            lore_debug!("Node {node_id} has no path in revision {revision}: {err}");
            String::new()
        }
    }
}

/// Reports `state` as a change of `action` of the file at `node_id` of `nodes`, the tree that
/// holds it: `state` itself, or for a deletion the parent the file was found in, as a revision
/// whose tree was started from another revision records a deletion under its first parent's node
/// ID. A deletion reports no content.
async fn history_state(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    nodes: &State,
    node_id: NodeID,
    action: u16,
) -> Result<(), FileHistoryError> {
    let path = if node_id != INVALID_NODE {
        nodes
            .node_path(repository.clone(), node_id)
            .await
            .forward::<FileHistoryError>("Invalid node")?
    } else {
        String::new()
    };

    let node = if node_id != INVALID_NODE && action != FileAction::Delete as u16 {
        Some(
            nodes
                .node(repository.clone(), node_id)
                .await
                .forward::<FileHistoryError>("Invalid node")?,
        )
    } else {
        None
    };

    // File properties
    let (file_action, file_metadata_hash, previous_revision, previous_node) = {
        if node_id != INVALID_NODE {
            let file_metadata_node_id = node::node_to_file_metadata(node_id);
            let file_metadata_block_index = NodeFileMetadataBlock::index(file_metadata_node_id);
            let file_metadata_node_index = NodeFileMetadata::index(file_metadata_node_id);

            let file_metadata_block = nodes
                .block_file_metadata(repository.clone(), file_metadata_block_index)
                .await
                .forward::<FileHistoryError>("Failed to deserialize metadata block")?;

            let file_metadata_block_reader = file_metadata_block.read();
            let file_metadata_node = file_metadata_block_reader.node(file_metadata_node_index);

            (
                file_metadata_node.action[0],
                file_metadata_node.metadata,
                file_metadata_node.revision[0],
                file_metadata_node.node[0],
            )
        } else {
            (
                FileAction::Delete as u16,
                Hash::default(),
                Hash::default(),
                INVALID_NODE,
            )
        }
    };

    // Action
    let action = if action == u16::MAX {
        file_action
    } else {
        action
    };

    let from_path = if action == FileAction::Move as u16 {
        node_path_in_revision(repository.clone(), previous_revision, previous_node).await
    } else {
        String::new()
    };

    // Revision
    event::LoreEvent::FileHistory(LoreFileHistoryEventData::new(
        path,
        from_path,
        repository.clone(),
        state.clone(),
        node.as_ref().map_or_default(|node| node.address),
        node.as_ref().map_or_default(|node| node.size),
        action as u32,
    ))
    .send();

    // Revision metadata
    let revision_metadata_hash = state.metadata_hash();
    if !revision_metadata_hash.is_zero() {
        let metadata = Metadata::deserialize(repository.clone(), revision_metadata_hash)
            .await
            .forward::<FileHistoryError>("Failed to deserialize metadata")?;

        event::metadata::send(&metadata);
    }

    // File metadata
    if !file_metadata_hash.is_zero() {
        let metadata = Metadata::deserialize(repository.clone(), file_metadata_hash)
            .await
            .forward::<FileHistoryError>("Failed to deserialize metadata")?;

        event::metadata::send(&metadata);
    }

    Ok(())
}

async fn history_parent(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent: Hash,
    relative_path: &RelativePath,
    options: HistoryOptions,
) -> Result<Option<(NodeID, LinkedState)>, FileHistoryError> {
    let mut depth = if options.depth > 0 { options.depth } else { 10 };

    let mut state = state.clone();
    let mut parent = parent;
    while depth > 0 && !parent.is_zero() {
        let state_parent = state::State::deserialize(repository.clone(), parent)
            .await
            .forward::<FileHistoryError>("Failed to deserialize state")?;

        let found = state_parent
            .locate_held_node_below_if_present(
                repository.clone(),
                ROOT_NODE,
                relative_path.as_str(),
            )
            .await
            .forward_any::<FileHistoryError>("finding the path")?
            .map(state::HeldNode::into_parts);
        if let Some((node, (_, held))) = &found {
            // A node a link holds is numbered in the linked repository, so `state`'s delta
            // cannot say what happened to it.
            if !Arc::ptr_eq(held, &state_parent) {
                return Ok(found);
            }
            // Node exists in 'state_parent' but not in 'state'.
            // What happened to it?

            let (nodes, node_id, action) = if state
                .node_deletion(repository.clone(), *node)
                .await
                .forward::<FileHistoryError>("Invalid node")?
                .is_some()
            {
                (&state_parent, *node, FileAction::Delete as u16)
            } else if let Some(change) =
                counterpart_change(&repository, &state_parent, *node, &state).await?
            {
                (&state, change.node, change.action)
            } else {
                (&state_parent, INVALID_NODE, FileAction::Delete as u16)
            };
            history_state(repository.clone(), state.clone(), nodes, node_id, action).await?;

            return Ok(found);
        }

        state = state_parent;
        depth -= 1;
        parent = state.parent_self();
    }

    Ok(None)
}

/// The change `state` records for the counterpart in its tree ([`state::find_counterpart_node`]) of
/// node `node` of `parent`, a parent revision; `None` where `state` holds no counterpart or records
/// no change of it.
async fn counterpart_change(
    repository: &Arc<RepositoryContext>,
    parent: &State,
    node: NodeID,
    state: &State,
) -> Result<Option<NodeDelta>, FileHistoryError> {
    let block_count = state
        .tree(repository.clone())
        .await
        .forward::<FileHistoryError>("Failed reading the revision's tree")?
        .block_count;
    let Some(counterpart) =
        state::find_counterpart_node(repository, parent, node, state, block_count)
            .await
            .forward::<FileHistoryError>("Failed finding the file in the revision")?
    else {
        return Ok(None);
    };
    state
        .node_change(repository.clone(), counterpart)
        .await
        .forward::<FileHistoryError>("Invalid node")
}

async fn find_start_revision(
    repository: Arc<RepositoryContext>,
    options: HistoryOptions,
) -> Result<Hash, FileHistoryError> {
    if options.revision.is_some() && options.branch.is_some() {
        return Err(FileHistoryError::internal("Invalid options"));
    }
    if let Some(revision_spec) = options.revision {
        return resolve_revision(repository, revision_spec.as_str()).await;
    }

    let branch = if options.branch.is_none() {
        let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<FileHistoryError>("Failed to deserialize current revision anchor")?;
        if current_branch.is_zero() {
            let metadata = repository::metadata_hash(repository.clone())
                .await
                .forward::<FileHistoryError>("Failed to load repository metadata")?;
            let metadata = repository::metadata(repository.clone(), metadata)
                .await
                .forward::<FileHistoryError>("Failed to load repository metadata")?;
            metadata.default_branch
        } else {
            current_branch
        }
    } else {
        let branch_name = options.branch.clone().unwrap_or_default();
        match branch::resolve(repository.clone(), branch_name.as_str()).await {
            Ok(resolved) => resolved.id,
            Err(err) if is_refusal(&err) => {
                return Err(err).forward_any("resolving branch");
            }
            Err(_err) => {
                return Err(FileHistoryError::from(BranchNotFound {
                    branch: branch_name,
                }));
            }
        }
    };

    if execution_context().globals().offline_or_local() {
        return Ok(branch::load_latest(repository.clone(), branch)
            .await
            .unwrap_or_default());
    }

    let remote_latest = if let Ok(remote) = repository.remote().await {
        branch::load_remote_latest(remote.clone(), repository.id, branch)
            .await
            .unwrap_or_default()
    } else {
        Hash::default()
    };
    if execution_context().globals().remote() {
        return Ok(remote_latest);
    }

    let local_latest = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();

    // Is there a latest?
    if remote_latest.is_zero() {
        return Ok(local_latest);
    }
    if remote_latest.is_zero() && local_latest.is_zero() {
        return Ok(Hash::default());
    }

    // Is remote latest the same as local latest?
    if remote_latest == local_latest {
        return Ok(remote_latest);
    }

    let mut remote_state = state::State::deserialize(repository.clone(), remote_latest)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;
    let local_state = state::State::deserialize(repository.clone(), local_latest)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;

    // Is local ahead of remote?
    if local_state.revision_number() > remote_state.revision_number() {
        return Ok(local_latest);
    }

    loop {
        let next_latest = remote_state.parent_self();

        // If a remote latest is encountered that matches the local latest, then remote
        // is ahead of local but there is no divergence.
        if next_latest == local_latest {
            break Ok(remote_latest);
        }

        remote_state = state::State::deserialize(repository.clone(), next_latest)
            .await
            .forward::<FileHistoryError>("Failed to deserialize state")?;

        // If remote revision number is less than the local revision number but
        // we haven't encountered the local revision number, there is a divergence.
        // In this case, show the local revision list until it's been pushed.
        if remote_state.revision_number() < local_state.revision_number() {
            break Ok(local_latest);
        }
    }
}

/// The revision a specifier names, in the forms `revision::resolve` accepts.
pub(crate) async fn resolve_revision(
    repository: Arc<RepositoryContext>,
    revision: &str,
) -> Result<Hash, FileHistoryError> {
    revision::resolve(
        repository,
        revision,
        execution_context().globals().search_location(),
    )
    .await
    .map_err(|err| {
        forward_refusal_or(err, "resolving the revision to query at", |err| {
            FileHistoryError::RevisionNotFound(
                RevisionNotFound {
                    revision: revision.to_string(),
                }
                .chain_err_from(err, "resolving the revision to query at"),
            )
        })
    })
}

/// Where [`history`] walks the history of a file on from.
struct HistoryStart {
    node: NodeID,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    /// The change to follow past an add marked as merged that [`history_start`] doesn't report,
    /// with the state of that change's revision ([`merged_add_history`]).
    next: Option<(HistoryRecord, Arc<State>)>,
}

/// Reports the change of the revision the history of the file at `path` starts from, and answers
/// where the walk goes on from, `None` where that change is the file's add.
async fn history_start(
    repository: Arc<RepositoryContext>,
    path: String,
    options: HistoryOptions,
) -> Result<Option<HistoryStart>, FileHistoryError> {
    let path_for_error = path.clone();
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, &path)
        .forward::<FileHistoryError>("resolving user path")?;

    // Start from the given revision, or from the branch latest (remote / local) if not given.
    let signature = find_start_revision(repository.clone(), options.clone()).await?;

    let state_start = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;

    // Handle case where history is queried for a file that was deleted in this revision by going to parent
    // revision(s), finding the node and if found, checking the delta block of this revision to see if that node was
    // deleted - if so print the history starting with delete and then going to parent revision(s) and continuing.
    let mut found = state_start
        .locate_held_node_below_if_present(repository.clone(), ROOT_NODE, relative_path.as_str())
        .await
        .forward_any::<FileHistoryError>("finding the path")?
        .map(state::HeldNode::into_parts);
    if found.is_none() {
        let parent = state_start.parent_self();

        found = history_parent(
            repository.clone(),
            state_start.clone(),
            parent,
            &relative_path,
            options.clone(),
        )
        .await?;
    }
    if found.is_none() {
        let parent = state_start.parent_other();

        found = history_parent(
            repository.clone(),
            state_start.clone(),
            parent,
            &relative_path,
            options.clone(),
        )
        .await?;
    }
    let Some((node, (repository, state))) = found else {
        return Err(FileNotFound {
            resource: path_for_error,
        }
        .into());
    };

    if let Some(node_delta) = state
        .node_change(repository.clone(), node)
        .await
        .forward::<FileHistoryError>("Deserialize delta block failed")?
    {
        if is_merged_add(&node_delta)
            && let Some(record) = merged_add_history(&repository, &state, node).await?
        {
            return Ok(Some(HistoryStart {
                node,
                repository,
                state,
                next: Some(record),
            }));
        }
        history_state(
            repository.clone(),
            state.clone(),
            &state,
            node,
            node_delta.action,
        )
        .await?;

        if node_delta.action == FileAction::Add as u16 {
            // Should not iterate history, added in this revision
            return Ok(None);
        }
    }

    Ok(Some(HistoryStart {
        node,
        repository,
        state,
        next: None,
    }))
}

/// Reports the history of the file at `path` along the first-parent line, reading and reporting
/// each earlier revision at the node ID the history records for the file there. Where the file has
/// no first-parent history before a revision, as for a file the second parent of a merge of
/// divergent history added, the walk takes the second-parent line. An add marked as merged
/// ([`is_merged_add`]) of a file whose recorded history holds the same file
/// ([`merged_add_history`]) is not reported, and the walk goes on along that history to the
/// revision that added the file.
pub async fn history(
    repository: Arc<RepositoryContext>,
    path: String,
    options: HistoryOptions,
) -> Result<(), FileHistoryError> {
    let mut limit = if options.length > 0 {
        options.length
    } else {
        100
    };

    let Some(HistoryStart {
        node: mut node_id,
        repository,
        mut state,
        mut next,
    }) = history_start(repository, path, options).await?
    else {
        return Ok(());
    };

    while limit > 0 {
        let HistoryRecord {
            action,
            node: node_id_next,
            ..
        } = if let Some((record, earlier)) = next.take() {
            state = earlier;
            record
        } else {
            let record = history_record(&repository, &state, node_id).await?;
            if record.revision.is_zero() {
                lore_debug!("Reached end of file history without ADD");
                break;
            }
            state = state::State::deserialize(repository.clone(), record.revision)
                .await
                .forward::<FileHistoryError>("Failed to deserialize state")?;
            record
        };
        node_id = node_id_next;

        if action == FileAction::Add as u16
            && state
                .node_change(repository.clone(), node_id)
                .await
                .forward::<FileHistoryError>("Failed to read the delta block")?
                .is_some_and(|delta| is_merged_add(&delta))
            && let Some(record) = merged_add_history(&repository, &state, node_id).await?
        {
            next = Some(record);
            continue;
        }

        history_state(repository.clone(), state.clone(), &state, node_id, action).await?;

        // Last?
        if action == FileAction::Add as u16 {
            break;
        }

        limit -= 1;
    }

    Ok(())
}

/// Whether `delta` records an add marked as merged.
fn is_merged_add(delta: &NodeDelta) -> bool {
    delta.action == FileAction::Add as u16 && delta.flags & crate::change::Flags::Merge != 0
}

/// The change before a revision that the history of a file records: its action, the revision and
/// the file's node there.
#[derive(Clone, Copy)]
struct HistoryRecord {
    action: u16,
    revision: Hash,
    node: NodeID,
}

impl HistoryRecord {
    /// The change `metadata`, a file's history, records in `slot`.
    fn at(metadata: &NodeFileMetadata, slot: usize) -> Self {
        Self {
            action: metadata.action[slot],
            revision: metadata.revision[slot],
            node: metadata.node[slot],
        }
    }
}

/// The change before `state` that the history of the file at `node` records, in the slot `lore
/// file history` follows ([`NodeFileMetadata::history_slot`]).
async fn history_record(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node: NodeID,
) -> Result<HistoryRecord, FileHistoryError> {
    let metadata_node = node::node_to_file_metadata(node);
    let block = state
        .block_file_metadata(
            repository.clone(),
            NodeFileMetadataBlock::index(metadata_node),
        )
        .await
        .forward::<FileHistoryError>("Failed to deserialize metadata block")?;
    let reader = block.read();
    let metadata = reader.node(NodeFileMetadata::index(metadata_node));
    lore_debug!(
        "Revision {} -> {} node {} file metadata {:?}",
        state.revision(),
        state.revision_number(),
        node,
        metadata
    );
    Ok(HistoryRecord::at(metadata, metadata.history_slot()))
}

/// The change before `state`, among the two the history of the file at `node` records, whose
/// revision holds the same file, one with its file ID, at the recorded node, with that revision's
/// state. `None` for a revision with one parent and where neither does.
async fn merged_add_history(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node: NodeID,
) -> Result<Option<(HistoryRecord, Arc<State>)>, FileHistoryError> {
    if state.parent_other().is_zero() {
        return Ok(None);
    }
    let file = state
        .node(repository.clone(), node)
        .await
        .forward::<FileHistoryError>("Failed reading the added file")?;
    if file.address.context.is_zero() {
        return Ok(None);
    }
    let metadata_node = node::node_to_file_metadata(node);
    let records = {
        let block = state
            .block_file_metadata(
                repository.clone(),
                NodeFileMetadataBlock::index(metadata_node),
            )
            .await
            .forward::<FileHistoryError>("Failed to deserialize metadata block")?;
        let reader = block.read();
        let metadata = reader.node(NodeFileMetadata::index(metadata_node));
        [0, 1].map(|slot| HistoryRecord::at(metadata, slot))
    };
    for record in records {
        if record.revision.is_zero() {
            continue;
        }
        let earlier = state::State::deserialize(repository.clone(), record.revision)
            .await
            .forward::<FileHistoryError>("Failed to deserialize state")?;
        let block_count = earlier
            .tree(repository.clone())
            .await
            .forward::<FileHistoryError>("Failed reading the earlier revision's tree")?
            .block_count;
        if state::same_file_counterpart(repository, &file, record.node, &earlier, block_count)
            .await
            .forward::<FileHistoryError>("Failed reading the file in the earlier revision")?
            .is_some()
        {
            return Ok(Some((record, earlier)));
        }
    }
    Ok(None)
}
