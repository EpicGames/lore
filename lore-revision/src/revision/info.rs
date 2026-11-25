// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Serialize;

use super::ResidentBlock;
use super::TypedBytes;
use crate::change;
use crate::change::FileAction;
use crate::change::Flags;
use crate::change::NodeChange;
use crate::errors::*;
use crate::event;
use crate::filter::FilterMode;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_warn;
use crate::metadata::Metadata;
use crate::node;
use crate::node::NodeDelta;
use crate::node::NodeFileMetadata;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::runtime::CancellationCheck;
use crate::runtime::execution_context;
use crate::state;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

/// Summary information about a single revision.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionInfoEventData {
    /// Repository identifier the revision belongs to.
    pub repository: RepositoryId,
    /// Revision hash signature.
    pub revision: Hash,
    /// Revision number.
    pub revision_number: u64,
    /// Parent revision hashes; the first is the direct parent and the second
    /// is the other parent of a merge, or zero when there is none.
    pub parent: [Hash; 2],
}

impl LoreRevisionInfoEventData {
    pub fn new(repository: RepositoryId, state: Arc<State>) -> Self {
        LoreRevisionInfoEventData {
            repository,
            revision: state.revision(),
            revision_number: state.revision_number(),
            parent: [state.parent_self(), state.parent_other()],
        }
    }
}

/// Per-file change information between a revision and its parent.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionInfoDeltaEventData {
    /// Path of the file relative to the repository root.
    pub path: LoreString,
    /// Size of the file in bytes.
    pub size: u64,
    /// Action applied to the file.
    pub action: LoreFileAction,
    /// Flag indicating the file content was modified.
    #[serde(with = "u8_as_bool")]
    pub flag_modify: u8,
    /// Flag indicating the change came from a merge.
    #[serde(with = "u8_as_bool")]
    pub flag_merged: u8,
    /// Flag indicating the entry is a file rather than a directory.
    #[serde(with = "u8_as_bool")]
    pub flag_file: u8,
    /// Path the file was moved from in this revision. Empty otherwise.
    pub from_path: LoreString,
}

impl LoreRevisionInfoDeltaEventData {
    pub fn new(path: &str, from_path: &str, delta: &NodeDelta, file: bool, size: u64) -> Self {
        LoreRevisionInfoDeltaEventData {
            path: path.into(),
            size,
            action: LoreFileAction::from(delta.action),
            flag_modify: ((delta.flags & Flags::Modify) != 0).into(),
            flag_merged: ((delta.flags & Flags::Merge) != 0).into(),
            flag_file: file.into(),
            from_path: from_path.into(),
        }
    }

    /// The entry for `change`, a change a walk found rather than one the revision's delta records.
    pub fn from_node_change(change: &NodeChange, file: bool, size: u64) -> Self {
        LoreRevisionInfoDeltaEventData {
            path: change.path().as_str().into(),
            size,
            action: LoreFileAction::from(change.action),
            flag_modify: change.flags.contains(Flags::Modify).into(),
            flag_merged: change.flags.contains(Flags::Merge).into(),
            flag_file: file.into(),
            from_path: change.move_source().map(|path| path.as_str()).into(),
        }
    }

    pub fn action_as_string_short(&self) -> &'static str {
        self.action.as_string_short()
    }

    pub fn merged_as_string_short(&self) -> &'static str {
        if self.flag_merged != 0 {
            return "(M)";
        }
        ""
    }
}

#[error_set]
pub enum InfoError {
    Cancelled,
    LinkPinsDiverge,
    AddressNotFound,
    Disconnected,
    FileNotFound,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    PayloadNotFound,
    RevisionNotFound,
    WriteRequired,
    InvalidArguments,
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

impl crate::event::EventError for InfoError {}

#[derive(Clone, Debug)]
pub struct InfoOptions {
    /// Optional revision signature
    pub signature: Option<String>,
    /// Optional include of delta information
    pub delta: bool,
    /// Optional include of file metadata information, implies --delta
    pub metadata: bool,
}

/// Report the revision `options` names, the metadata it records, and the delta it
/// carries when asked for, followed by the changes inside the links whose pin moved from its
/// first parent, regardless of the instance's view.
///
/// A revision naming metadata that cannot be read is reported without the branch, date and
/// message that blob carries, and warned about, rather than leaving those fields silently
/// empty.
pub(crate) async fn info(
    repository: Arc<RepositoryContext>,
    options: InfoOptions,
) -> Result<(), InfoError> {
    let signature = if let Some(signature) = options.signature {
        revision::resolve(
            repository.clone(),
            signature,
            execution_context().globals().search_location(),
        )
        .await
        .forward::<InfoError>("invalid revision")?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<InfoError>("deserializing current anchor")?;
        current_revision
    };

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<InfoError>("deserializing state")?;

    event::LoreEvent::RevisionInfo(LoreRevisionInfoEventData::new(repository.id, state.clone()))
        .send();

    if let Some(metadata) =
        revision::reported_metadata(repository.clone(), state.metadata_hash()).await
    {
        event::metadata::send(&metadata);
    }

    if options.delta || options.metadata {
        let mut parent_state: Option<Arc<State>> = None;
        let mut moved_links = Vec::new();
        // Boxed, and only for --metadata: the blocks would grow every info's future.
        let mut metadata_blocks = options.metadata.then(Box::<MetadataBlocks>::default);

        if let Ok(delta_buffer) = state
            .delta_block(repository.clone())
            .await
            .forward::<InfoError>("deserializing delta block")
        {
            let delta_buffer = delta_buffer.to_aligned::<NodeDelta>();
            for delta in delta_buffer.as_type_slice::<NodeDelta>().iter() {
                let is_delete = delta.action == FileAction::Delete as u16;
                let is_move = delta.action == FileAction::Move as u16;
                let is_parent_loaded = parent_state.is_some();
                if is_delete && !is_parent_loaded {
                    parent_state = Some(
                        state::State::deserialize(repository.clone(), state.parent_self())
                            .await
                            .forward::<InfoError>("deserializing state")?,
                    );
                } else if is_move && !is_parent_loaded {
                    parent_state =
                        state::State::deserialize(repository.clone(), state.parent_self())
                            .await
                            .inspect_err(|err| {
                                lore_debug!(
                                    "Failed to deserialize parent state for move source: {err}"
                                );
                            })
                            .ok();
                }

                let node_id = delta.node;

                let path;
                let file;
                let size;

                if is_delete {
                    let Some(parent_state) = parent_state.clone() else {
                        return Err(InfoError::internal("deserializing state"));
                    };

                    path = parent_state
                        .node_path(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node path")?;
                    file = parent_state
                        .node(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node block")?
                        .is_file();
                    size = 0;
                } else {
                    path = state
                        .node_path(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node path")?;

                    let node = state
                        .node(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node block")?;

                    file = node.is_file();
                    size = node.size;
                    // The delta records a link whose pin moved as its node, and nothing inside it.
                    if node.is_link() && delta.action != FileAction::Add as u16 {
                        let Ok(mount) = RelativePath::from_str(&path);
                        moved_links.push(mount);
                    }
                }

                let from_path = match parent_state.as_ref() {
                    Some(parent_state) if is_move => {
                        move_source(&repository, &state, parent_state, node_id, file).await
                    }
                    _ => String::new(),
                };

                event::LoreEvent::RevisionInfoDelta(LoreRevisionInfoDeltaEventData::new(
                    &path, &from_path, delta, file, size,
                ))
                .send();

                if let Some(blocks) = metadata_blocks.as_deref_mut() {
                    let (metadata_state, resident) = if is_delete {
                        (
                            parent_state
                                .as_ref()
                                .ok_or_else(|| InfoError::internal("deserializing state"))?,
                            &mut blocks.from,
                        )
                    } else {
                        (&state, &mut blocks.to)
                    };
                    send_file_metadata(resident, &repository, metadata_state, node_id).await?;
                }
            }

            if !moved_links.is_empty() {
                // Boxed: few revisions move a link, and the walk would grow every info's future.
                Box::pin(send_moved_link_deltas(
                    &repository,
                    parent_state,
                    &state,
                    &moved_links,
                    metadata_blocks.as_deref_mut(),
                ))
                .await?;
            }
        }
    }

    Ok(())
}

/// The file metadata blocks [`info`] read last, one for each side of a change, so the entries
/// after them read each block once: a state holds a block only while something else does.
#[lore_macro::test_pub]
#[derive(Default)]
struct MetadataBlocks {
    /// The block of the revision a deleted node stood in, its first parent's.
    from: ResidentBlock,
    /// The block of the revision every other node stands in.
    to: ResidentBlock,
}

/// Sends a `RevisionInfoDelta` event for each change inside the links at `mounts`, with its file
/// metadata, read through `metadata`, when that is given, as `revision diff` finds them from the
/// first parent but through no view. That walk reports a link the caller may not read, or one
/// remounted, as its node alone, so nothing inside it is listed. A walk that fails lists none, with
/// a warning. A node or metadata read that fails after the walk stops the listing there, with a
/// warning that it is incomplete, rather than holding every entry's node and metadata until all of
/// them have been read. A cancelled walk fails the command instead, and cancellation is checked
/// before each entry is listed, so a command cancelled while it lists does not read as complete.
#[lore_macro::test_pub]
async fn send_moved_link_deltas(
    repository: &Arc<RepositoryContext>,
    parent_state: Option<Arc<State>>,
    state: &Arc<State>,
    mounts: &[RelativePath],
    mut metadata: Option<&mut MetadataBlocks>,
) -> Result<(), InfoError> {
    let changes = async {
        let parent_state = match parent_state {
            Some(parent_state) => parent_state,
            None => State::deserialize(repository.clone(), state.parent_self()).await?,
        };
        let walk_repository = repository.clone();
        let state = state.clone();
        let from = mounts_parent(mounts);
        let mut walk = state::ChangeStream::spawn(async move |changes| {
            state::diff(
                walk_repository.clone(),
                parent_state,
                walk_repository,
                state,
                (!from.is_empty()).then_some(from),
                None,
                &changes,
                FilterMode::empty(),
            )
            .await
        });
        let mut inside = Vec::new();
        while let Some(change) = walk.next().await {
            if mounts
                .iter()
                .any(|mount| change.path().is_below_ignore_case(mount))
            {
                inside.push(change);
            }
        }
        walk.finish().await?;
        state::detect_and_coalesce_moves(&mut inside);
        change::sort_by_path(&mut inside);
        Ok::<_, StateError>(inside)
    };
    let changes = match changes.await {
        Ok(changes) => changes,
        Err(err) if err.is_cancelled() => {
            return Err(err).forward::<InfoError>("listing the changes inside the links");
        }
        Err(err) => {
            lore_warn!("Changes inside the links not listed: {err}");
            return Ok(());
        }
    };
    let cancellation = CancellationCheck::current();
    for change in &changes {
        cancellation.check()?;
        match send_linked_delta(change, metadata.as_deref_mut()).await {
            Ok(()) => {}
            Err(err) if err.is_cancelled() => return Err(err),
            Err(err) => {
                lore_warn!("Changes inside the links not all listed: {err}");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The deepest directory holding every path of `mounts`, the walk's root, so the walk leaves out
/// the rest of the tree.
fn mounts_parent(mounts: &[RelativePath]) -> RelativePath {
    let mut parent = mounts
        .first()
        .map_or_else(RelativePath::new, RelativePath::parent_path);
    while !parent.is_empty()
        && !mounts
            .iter()
            .all(|mount| mount.is_below_ignore_case(&parent))
    {
        parent.pop();
    }
    parent
}

/// Sends a `RevisionInfoDelta` event for a change inside a link, read from the side of the change
/// that holds the node, with its file metadata, read through `metadata`, when that is given.
async fn send_linked_delta(
    change: &NodeChange,
    metadata: Option<&mut MetadataBlocks>,
) -> Result<(), InfoError> {
    let side = change.resolved_side();
    let node = side
        .get_node()
        .await
        .forward::<InfoError>("accessing node block")?;
    let size = if change.action == FileAction::Delete {
        0
    } else {
        node.size
    };

    event::LoreEvent::RevisionInfoDelta(LoreRevisionInfoDeltaEventData::from_node_change(
        change,
        node.is_file(),
        size,
    ))
    .send();

    if let Some(blocks) = metadata {
        let resident = if change.action == FileAction::Delete {
            &mut blocks.from
        } else {
            &mut blocks.to
        };
        send_file_metadata(
            resident,
            &side.mapping.repository,
            &side.mapping.state,
            side.mapping.node,
        )
        .await?;
    }
    Ok(())
}

/// Sends the file metadata `node_id` carries in `state`, if it carries any, reading its block
/// through `resident`.
async fn send_file_metadata(
    resident: &mut ResidentBlock,
    repository: &Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
) -> Result<(), InfoError> {
    let metadata_node_id = node::node_to_file_metadata(node_id);
    let metadata_hash = resident
        .of(repository, state, metadata_node_id)
        .await
        .forward::<InfoError>("deserializing metadata block")?
        .read()
        .node(NodeFileMetadata::index(metadata_node_id))
        .metadata;

    if !metadata_hash.is_zero()
        && let Ok(metadata) = Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<InfoError>("deserializing file metadata")
    {
        event::metadata::send(&metadata);
    }
    Ok(())
}

/// Boxed version of [`info`] for cross-crate use.
pub fn info_boxed(
    repository: Arc<RepositoryContext>,
    options: InfoOptions,
) -> crate::BoxFuture<'static, Result<(), InfoError>> {
    Box::pin(info(repository, options))
}

/// The path `parent`, the first parent of `state`, holds node `node_id` of `state` at, the source
/// of the move the delta of `state` records: the path of its counterpart in `parent`
/// ([`state::find_counterpart_node`]), as a tree built on another revision than its first parent
/// does not share its node IDs. A directory's move is recorded only by a tree that shares them, so
/// its own ID names it there. Empty where `parent` holds no counterpart or its path can't be read.
async fn move_source(
    repository: &Arc<RepositoryContext>,
    state: &State,
    parent: &State,
    node_id: NodeID,
    is_file: bool,
) -> String {
    let counterpart = if is_file {
        match parent.tree(repository.clone()).await {
            Ok(tree) => {
                state::find_counterpart_node(repository, state, node_id, parent, tree.block_count)
                    .await
                    .inspect_err(|err| {
                        lore_debug!(
                            "Node {node_id} has no counterpart in the parent revision: {err}"
                        );
                    })
                    .ok()
                    .flatten()
            }
            Err(err) => {
                lore_debug!("Failed reading the parent revision's tree: {err}");
                None
            }
        }
    } else {
        Some(node_id)
    };
    let Some(counterpart) = counterpart else {
        return String::new();
    };
    parent
        .node_path(repository.clone(), counterpart)
        .await
        .inspect_err(|err| {
            lore_debug!("Node {counterpart} has no path in the parent revision: {err}");
        })
        .unwrap_or_default()
}
