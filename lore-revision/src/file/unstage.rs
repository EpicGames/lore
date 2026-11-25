// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::file::stage::route_layer_paths;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::layer;
use crate::link;
use crate::link::LinkTracker;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::resolve_user_paths;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::is_reserved_node_name;
use crate::state;
use crate::state::Clear;
use crate::state::FoundNode;
use crate::state::LinkPin;
use crate::state::Mark;
use crate::state::NodeMapping;
use crate::state::OwnFlags;
use crate::state::State;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::util;
use crate::util::path::RelativePath;

/// Data for the event emitted when an unstage operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageBeginEventData {
    /// Number of paths requested for unstaging.
    pub path_count: usize,
}

/// Running counts of items processed during an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageCountData {
    /// Number of directories and links that were unstaged.
    pub directory_unstaged_count: u64,
    /// Number of directories that were discarded.
    pub directory_discarded_count: u64,
    /// Number of files that were unstaged.
    pub file_unstaged_count: u64,
    /// Number of files that were discarded.
    pub file_discarded_count: u64,
    /// Total number of items processed.
    pub total_count: u64,
}

/// Data for the progress event emitted periodically during an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileUnstageCountData,
}

/// Data for the event emitted when an unstage operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileUnstageCountData,
}

/// Data for the event identifying the repository and revision involved in an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageRevisionEventData {
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision the files are unstaged against.
    pub revision: Hash,
}

/// Data for the event emitted for each file affected by an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageFileEventData {
    /// Path of the file, relative to the root of the working tree.
    pub path: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
}

#[error_set]
pub enum UnstageError {
    Cancelled,
    LinkPinsDiverge,
    InvalidArguments,
    InvalidPath,
    InvalidNodeHierarchy,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    RevisionNotFound,
    WriteRequired,
    AddressNotFound,
    Oversized,
    Disconnected,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    PayloadNotFound,
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
    FileNotFound,
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

impl EventError for UnstageError {
    fn translated(&self) -> LoreError {
        match self {
            UnstageError::InvalidArguments(_) | UnstageError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Default)]
pub struct UnstageStats {
    pub directory_unstaged_count: AtomicU64,
    pub directory_discarded_count: AtomicU64,
    pub file_unstaged_count: AtomicU64,
    pub file_discarded_count: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnstageOptions {
    /// Single node, no recursion
    pub single_node: bool,
    /// Discard the work a staged link add or removal would delete or overwrite at its mount,
    /// which unstaging otherwise refuses to.
    pub discard_local: bool,
    /// Unstage past the ignore rules and the view, which leave out what they exclude.
    pub past_filter: bool,
}

pub async fn unstage(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    options: UnstageOptions,
) -> Result<(), UnstageError> {
    let relative_paths = resolve_user_paths(&repository, &paths).await?;

    let layers = layer::list(repository.clone())
        .await
        .forward::<UnstageError>("Failed to list layers")?;
    let (parent_paths, layer_jobs) = route_layer_paths(&layers, relative_paths);

    event::LoreEvent::FileUnstageBegin(LoreFileUnstageBeginEventData {
        path_count: parent_paths.len() + layer_jobs.iter().map(|(_, r)| r.len()).sum::<usize>(),
    })
    .send();

    let stats = Arc::new(UnstageStats::default());

    if !parent_paths.is_empty() {
        unstage_parent(
            repository.clone(),
            token,
            parent_paths,
            options,
            stats.clone(),
        )
        .await?;
    }

    for (layer_index, remains) in layer_jobs {
        Box::pin(unstage_from_layer(
            repository.clone(),
            token,
            &layers[layer_index],
            &remains,
            options,
            stats.clone(),
        ))
        .await?;
    }

    let directory_unstaged_count = stats.directory_unstaged_count.load(Ordering::Relaxed);
    let directory_discarded_count = stats.directory_discarded_count.load(Ordering::Relaxed);
    let file_unstaged_count = stats.file_unstaged_count.load(Ordering::Relaxed);
    let file_discarded_count = stats.file_discarded_count.load(Ordering::Relaxed);

    event::LoreEvent::FileUnstageEnd(LoreFileUnstageEndEventData {
        count: LoreFileUnstageCountData {
            directory_unstaged_count,
            directory_discarded_count,
            file_unstaged_count,
            file_discarded_count,
            total_count: directory_unstaged_count
                + directory_discarded_count
                + file_unstaged_count
                + file_discarded_count,
        },
    })
    .send();

    Ok(())
}

/// Unstages `paths` against the repository's own current and staged states.
///
/// One operation covers every path: unstaging reads the working copy to resolve the case each path
/// is held in, and one per path would freeze a filesystem per path. It is opened as one that
/// writes: putting a staged link change back realizes the pinned content at its mount through it.
async fn unstage_parent(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: Vec<RelativePath>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
) -> Result<(), UnstageError> {
    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<UnstageError>("Failed to deserialize current revision anchor")?;
    let Ok(staged_revision) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .ok_or("no staged revision")
    else {
        lore_debug!("No staged state when unstaging, nothing to do");
        return Ok(());
    };

    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward_with::<UnstageError, _>(|| {
            format!("Failed to deserialize revision state {current_revision}")
        })?;

    let state_staged =
        State::deserialize_staged(repository.clone(), staged_revision, current_revision)
            .await
            .forward_with::<UnstageError, _>(|| {
                format!("Failed to deserialize revision state {staged_revision}")
            })?;
    state_staged.hold_current_where_staged_on(&state_current);

    let discard = Arc::new(Discards::default());
    let link_tracker = LinkTracker::new();
    let is_merge_or_cherry_pick_or_revert = state_staged.is_merge_or_cherry_pick_or_revert();

    // One operation covers the unstaging and storing what it leaves staged: an SWFS instance
    // serves links from what is stored when the operation finalizes, so a link whose repin is
    // unstaged has to be stored by then (ADR-00042).
    let signature = with_operation(repository.file_system(), async |operation| {
        unstage_each_path(UnstagePaths {
            operation: &operation,
            repository: &repository,
            state_current: &state_current,
            state_staged: &state_staged,
            paths: &paths,
            discard: &discard,
            options,
            stats: &stats,
            link_tracker: &link_tracker,
        })
        .await?;

        let mut clear = !is_merge_or_cherry_pick_or_revert
            && !state_staged
                .node_has_staged_or_dirty_children(repository.clone(), ROOT_NODE)
                .await
                .forward::<UnstageError>("Failed to find subnode")?;

        if clear {
            lore_debug!("Unstaged all, clean by removing staged state anchor");
            if crate::instance::delete_staged_anchor(&repository)
                .await
                .is_err()
            {
                clear = false;
            }
        }

        if !clear {
            discard_nodes(&discard).await?;
        }

        process_link_unstage_updates(
            repository.clone(),
            token,
            state_staged.clone(),
            link_tracker.clone(),
        )
        .await?;

        let total_count = stats.directory_unstaged_count.load(Ordering::Relaxed)
            + stats.directory_discarded_count.load(Ordering::Relaxed)
            + stats.file_unstaged_count.load(Ordering::Relaxed)
            + stats.file_discarded_count.load(Ordering::Relaxed);

        if total_count == 0 || clear {
            lore_debug!(
                "Nothing unstaged or nothing remains staged, not serializing new staged state and anchor"
            );
            return Ok(None);
        }

        state_staged.mark_dirty();

        if state_staged.holds_promoted_moves() {
            Box::pin(crate::file::stage::stage_promoted_moves_of_call(
                &repository,
                &state_staged,
                None,
            ))
            .await
            .forward::<UnstageError>("Failed to stage the move of a directory holding a staged link")?;
        }
        let signature = state_staged
            .serialize(repository.clone(), token)
            .await
            .forward::<UnstageError>("Failed to serialize staged revision state")?;
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<UnstageError>("Failed to serialize staged anchor")?;
        Ok::<_, UnstageError>(Some(signature))
    })
    .await?;

    if let Some(signature) = signature {
        event::LoreEvent::FileUnstageRevision(LoreFileUnstageRevisionEventData {
            repository: repository.id,
            revision: signature,
        })
        .send();
    }

    Ok(())
}

/// Unstage the mount-relative `remains` against the layer's own current and staged states.
///
/// Each `remain` resolves under the layer's `source_path` for the state lookup. One operation
/// covers the whole layer, and writes, for the same reasons [`unstage_parent`] opens one for all
/// its paths.
async fn unstage_from_layer(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    layer: &layer::Layer,
    remains: &[RelativePath],
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
) -> Result<(), UnstageError> {
    if layer.staged_revision().is_none() {
        lore_debug!(
            "Layer at {} holds no staged state, nothing to unstage",
            layer.target_path
        );
        return Ok(());
    }

    let source_path = RelativePath::new_from_initial_path(&layer.source_path)
        .forward_with::<UnstageError, _>(|| {
            format!("Invalid layer source path {}", layer.source_path)
        })?;

    let layer_state = layer
        .deserialize_current_and_staged(repository.clone())
        .await
        .forward::<UnstageError>("Failed to deserialize layer state")?;

    let discard = Arc::new(Discards::default());
    let link_tracker = LinkTracker::new();

    with_operation(repository.file_system(), async |operation| {
        for remain in remains {
            Box::pin(unstage_path(
                operation.clone(),
                layer_state.repository.clone(),
                layer_state.state_current.clone(),
                layer_state.state_staged.clone(),
                source_path.join(remain.as_str()),
                discard.clone(),
                options,
                stats.clone(),
                link_tracker.clone(),
            ))
            .await?;
        }
        Ok::<_, UnstageError>(())
    })
    .await?;

    discard_nodes(&discard).await?;

    process_link_unstage_updates(
        layer_state.repository.clone(),
        token,
        layer_state.state_staged.clone(),
        link_tracker,
    )
    .await?;

    if execution_context().globals().dry_run() {
        return Ok(());
    }
    if layer_state.state_staged.holds_promoted_moves() {
        Box::pin(crate::file::stage::stage_promoted_moves_of_call(
            &layer_state.repository,
            &layer_state.state_staged,
            None,
        ))
        .await
        .forward::<UnstageError>("Failed to stage the move of a directory holding a staged link")?;
    }

    let signature = layer::store_staged_or_clear(repository, token, layer, &layer_state)
        .await
        .forward::<UnstageError>("Failed to store layer staged state")?;

    event::LoreEvent::FileUnstageRevision(LoreFileUnstageRevisionEventData {
        repository: layer_state.repository.id,
        revision: signature,
    })
    .send();

    Ok(())
}

/// What unstaging each path needs: the trees it rewrites and the filesystem operation it resolves
/// path cases through and puts a staged link change back through.
struct UnstagePaths<'a> {
    operation: &'a Arc<InstanceOperationImpl>,
    repository: &'a Arc<RepositoryContext>,
    state_current: &'a Arc<State>,
    state_staged: &'a Arc<State>,
    paths: &'a [RelativePath],
    discard: &'a Arc<Discards>,
    options: UnstageOptions,
    stats: &'a Arc<UnstageStats>,
    link_tracker: &'a Arc<LinkTracker>,
}

/// Unstage each path in turn, reporting progress while one runs.
async fn unstage_each_path(args: UnstagePaths<'_>) -> Result<(), UnstageError> {
    let UnstagePaths {
        operation,
        repository,
        state_current,
        state_staged,
        paths,
        discard,
        options,
        stats,
        link_tracker,
    } = args;
    for relative_path in paths {
        let relative_path = relative_path.clone();

        lore_debug!("Unstage options: {:?}", options);

        let mut task = lore_spawn!(unstage_path(
            operation.clone(),
            repository.clone(),
            state_current.clone(),
            state_staged.clone(),
            relative_path,
            discard.clone(),
            options,
            stats.clone(),
            link_tracker.clone(),
        ));

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let result = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let directory_unstaged_count = stats.directory_unstaged_count.load(Ordering::Relaxed);
                    let directory_discarded_count = stats.directory_discarded_count.load(Ordering::Relaxed);
                    let file_unstaged_count = stats.file_unstaged_count.load(Ordering::Relaxed);
                    let file_discarded_count = stats.file_discarded_count.load(Ordering::Relaxed);

                    event::LoreEvent::FileUnstageProgress(LoreFileUnstageProgressEventData {
                        count: LoreFileUnstageCountData {
                            directory_unstaged_count,
                            directory_discarded_count,
                            file_unstaged_count,
                            file_discarded_count,
                            total_count: directory_unstaged_count
                                + directory_discarded_count
                                + file_unstaged_count
                                + file_discarded_count,
                        },
                    }).send();
                },
                result = &mut task => {
                    break result.internal("Recursion task failed").map_err(UnstageError::from)?;
                }
            }
        };

        result?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn unstage_path(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    relative_path: RelativePath,
    discard: Arc<Discards>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    lore_debug!(
        "Unstaging path: {}/{}",
        repository.path_for_display(),
        relative_path.as_str(),
    );

    let relative_path = if relative_path.is_empty() {
        relative_path
    } else {
        repository.require_path()?;
        let resolved = util::fs::filesystem_path(&operation, "", &relative_path, None).await;
        resolved.unwrap_or(relative_path)
    };

    let parent_states = repository.filter.parent_exclusion_states(&relative_path);

    // Path is repository root, unstage as directory
    if relative_path.is_empty() {
        if options.single_node {
            return Ok(());
        }

        lore_debug!("Unstaging the repository from root");

        return unstage_directory(
            operation,
            NodeMapping {
                repository: repository.clone(),
                state: state_staged.clone(),
                path: relative_path.clone(),
                node: ROOT_NODE,
            },
            state_current.clone(),
            discard.clone(),
            options,
            stats.clone(),
            link_tracker.clone(),
            parent_states,
        )
        .await;
    }

    let Some(target) = resolve_unstage_target(
        repository,
        state_current,
        state_staged,
        &relative_path,
        &link_tracker,
    )
    .await?
    else {
        lore_debug!("Path {} names no node", relative_path.as_str());
        return Ok(());
    };

    let (flags, replaced) =
        staged_type_change(&target.repository, &target.state_staged, target.node_id).await?;
    let unstaged = replaced.unwrap_or(target.node_id);
    let deleted = replaced.is_some() || flags.contains(NodeFlags::StagedDelete);
    let at = NodeMapping {
        repository: target.repository.clone(),
        state: target.state_staged.clone(),
        path: relative_path,
        node: target.node_id,
    };
    match replaced {
        Some(replaced) => {
            unstage_replacement(
                operation,
                at,
                replaced,
                target.state_current,
                discard,
                options,
                stats,
                link_tracker.clone(),
                parent_states,
            )
            .await?;
        }
        None => {
            unstage_node_recurse(
                operation,
                at,
                target.state_current,
                discard,
                options,
                stats,
                link_tracker.clone(),
                parent_states,
            )
            .await?;
        }
    }
    if deleted {
        keep_directories_above(&target.repository, &target.state_staged, unstaged).await?;
    }
    Ok(())
}

/// Where the unstage of `node_id`, staged for deletion, left it unstaged, turns each directory
/// above it staged for deletion into a directory staged for what lies below it alone, the topmost
/// one included: the commit then deletes the rest below them, and keeps them and the unstaged node,
/// but where a directory above them that the working tree no longer holds carries a staged change
/// of its own, which they go with. Reads each directory up to the root, as one staged otherwise can
/// stand between two staged for deletion, and lists none.
async fn keep_directories_above(
    repository: &Arc<RepositoryContext>,
    state_staged: &State,
    node_id: NodeID,
) -> Result<(), UnstageError> {
    let node = state_staged
        .node(repository.clone(), node_id)
        .await
        .forward::<UnstageError>("Failed deserializing state node block")?;
    if node.is_staged() {
        return Ok(());
    }
    let mut at = node.parent;
    while at.is_valid_node_id() {
        let block = state_staged
            .block(repository.clone(), NodeBlock::index(at))
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        let (deleted, parent) = {
            let directory = block.node(Node::index(at));
            (directory.is_staged_delete(), directory.parent)
        };
        if deleted {
            state_staged.node_clear_own(&block, at, OwnFlags::StagedToBare);
        }
        at = parent;
    }
    Ok(())
}

/// The flags of `newest`, the newest node of its name, and the node of another kind that a staged
/// type change adding `newest` replaces, which is staged for delete. Nodes are prepended, so the
/// replaced one follows the added one.
async fn staged_type_change(
    repository: &Arc<RepositoryContext>,
    state: &State,
    newest: NodeID,
) -> Result<(NodeFlags, Option<NodeID>), UnstageError> {
    let added = state
        .node(repository.clone(), newest)
        .await
        .forward::<UnstageError>("Failed deserializing state node block")?;
    let flags = NodeFlags::from_bits_retain(added.flags);
    if !added.is_staged_add() && !added.is_dirty_add() {
        return Ok((flags, None));
    }
    let mut cycle = SiblingCycleGuard::new(added.parent);
    let mut next = added.sibling();
    while let Some(id) = next {
        let node = state
            .node(repository.clone(), id)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        node.walk_step(id, added.parent, &mut cycle)
            .forward::<UnstageError>("Invalid node hierarchy")?;
        if node.name_hash == added.name_hash {
            let replaced = node.is_staged_delete() && node.node_type() != added.node_type();
            return Ok((flags, replaced.then_some(id)));
        }
        next = node.sibling();
    }
    Ok((flags, None))
}

/// Whether the current revision's registry holds the link `link_id` at `node_id`.
async fn link_committed(
    repository: &Arc<RepositoryContext>,
    state_current: &State,
    link_id: RepositoryId,
    node_id: NodeID,
) -> Result<bool, UnstageError> {
    match state_current
        .link_find(repository.clone(), link_id, node_id)
        .await
    {
        Ok(_) => Ok(true),
        Err(err) if err.is_link_not_found() => Ok(false),
        Err(err) => Err(err).forward::<UnstageError>("Failed to find link registry entry"),
    }
}

/// Whether unstaging discards the local changes a staged link change would otherwise refuse to
/// overwrite.
fn overwrite_local(options: UnstageOptions) -> bool {
    options.discard_local || execution_context().globals().force()
}

/// The node a path names, and the trees it lives in.
struct UnstageTarget {
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    node_id: NodeID,
}

/// The node `node_path` names, or `None` when it names none.
///
/// A path that reaches into a link names a node of the repository the link mounts, so every link
/// on the way is resolved and tracked, and the node answered with is one the innermost
/// repository holds, in the state the tracker reserializes. A walk below this node identifies
/// each of its own by node id and needs no path resolved.
async fn resolve_unstage_target(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    node_path: &RelativePath,
    link_tracker: &Arc<LinkTracker>,
) -> Result<Option<UnstageTarget>, UnstageError> {
    let found = match state_staged
        .locate_node(repository.clone(), node_path.as_str())
        .await
    {
        Ok(found) => found,
        Err(err) if err.is_node_not_found() => return Ok(None),
        Err(err) => return Err(err).forward::<UnstageError>("Failed to find subnode"),
    };

    if let Some(node_id) = found.own_node() {
        return Ok(Some(UnstageTarget {
            repository,
            state_current,
            state_staged,
            node_id,
        }));
    }

    lore_debug!(
        "Transition into linked repository: from {} to {}",
        repository.id,
        found.identity().repository,
    );

    let chain = crate::link::resolve_tracked_link_chain(
        NodeMapping::root(repository.clone(), state_staged.clone()),
        state_current,
        node_path.clone(),
        link_tracker,
    )
    .await
    .forward::<UnstageError>("Failed to resolve link chain")?;

    let repository = chain.innermost.repository.clone();
    let state_staged = chain.innermost.state.clone();
    let state_current = State::deserialize(
        repository.clone(),
        chain
            .levels
            .last()
            .map_or_else(|| state_staged.revision(), |level| level.old_signature),
    )
    .await
    .forward::<UnstageError>("Failed to deserialize revision state")?;

    let node_id = state_staged
        .locate_node_below(
            repository.clone(),
            chain.innermost.node,
            chain.remainder_path.as_str(),
        )
        .await
        .and_then(FoundNode::into_own_node)
        .forward::<UnstageError>("Failed to find node in linked repository")?;

    Ok(Some(UnstageTarget {
        repository,
        state_current,
        state_staged,
        node_id,
    }))
}

/// Each child of `at`'s path steps from the verdict `states` carries.
#[allow(clippy::too_many_arguments)]
async fn unstage_directory(
    operation: Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    discard: Arc<Discards>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
    states: FilterStates,
) -> Result<(), UnstageError> {
    let NodeMapping {
        repository,
        state: state_staged,
        path: directory_path,
        node: directory_node,
    } = at;
    lore_trace!(
        "Unstaging directory: path='{}', node={}, repository={}",
        directory_path.as_str(),
        directory_node,
        repository.id
    );

    let mut children = StateNodeChildrenWithNameIterator::new(
        state_staged.clone(),
        repository.clone(),
        directory_node,
    )
    .await
    .forward::<UnstageError>("Failed to list directory node children")?;
    let mut staged_deletes = None;
    // A replaced node follows the add that replaced it, which unstages both.
    let mut unstaged_replaced = Vec::new();

    // TODO(vri): UCS-12399 - Convert to separate tasks
    while let Some((child_node_id, child_node, child_name)) = children
        .next()
        .await
        .forward::<UnstageError>("Failed to list directory node children")?
    {
        // Takes the name by value so its block read lock ends here, rather than reaching the
        // unstage of the child below (see NodeNameLock docs).
        let child_node_path = directory_path.join(child_name);
        if unstaged_replaced.contains(&child_node_id) {
            continue;
        }

        let replacement = if child_node.is_staged_add() || child_node.is_dirty_add() {
            let staged_deletes = match staged_deletes {
                Some(ref names) => names,
                None => staged_deletes.insert(
                    state_staged
                        .node_staged_delete_name_hashes(repository.clone(), directory_node)
                        .await
                        .forward::<UnstageError>("Failed to list directory node children")?,
                ),
            };
            if staged_deletes.contains(&child_node.name_hash) {
                staged_type_change(&repository, &state_staged, child_node_id)
                    .await?
                    .1
            } else {
                None
            }
        } else {
            None
        };

        lore_trace!(
            "Unstaging child node: node={}, path='{}' in repository {}",
            child_node_id,
            child_node_path.as_str(),
            repository.id
        );

        let at = NodeMapping {
            repository: repository.clone(),
            state: state_staged.clone(),
            path: child_node_path,
            node: child_node_id,
        };
        if let Some(replaced) = replacement {
            unstaged_replaced.push(replaced);
            unstage_replacement(
                operation.clone(),
                at,
                replaced,
                state_current.clone(),
                discard.clone(),
                options,
                stats.clone(),
                link_tracker.clone(),
                states,
            )
            .await?;
        } else {
            unstage_node_recurse(
                operation.clone(),
                at,
                state_current.clone(),
                discard.clone(),
                options,
                stats.clone(),
                link_tracker.clone(),
                states,
            )
            .await?;
        }
    }

    Ok(())
}

/// Clears the staged flags of `starting_parent`, and of each directory above it, while it holds
/// no staged child. With `stop_at_staged_add`, it stops at a staged add, which it leaves staged;
/// without it, it clears the dirty flags too, where no dirty child keeps them. Returns whether it
/// cleared `starting_parent`.
async fn unstage_parent_chain(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    starting_parent: NodeID,
    link_tracker: Arc<LinkTracker>,
    stop_at_staged_add: bool,
) -> Result<bool, UnstageError> {
    let mut parent_node_id = starting_parent;
    let mut cleared = false;
    while !state_staged
        .node_has_staged_children(repository.clone(), parent_node_id)
        .await
        .forward::<UnstageError>("Failed to check node children")?
    {
        lore_trace!("Unstage parent node {parent_node_id}");

        let parent_block_index = NodeBlock::index(parent_node_id);
        let parent_node_index = Node::index(parent_node_id);
        let parent_block = state_staged
            .block(repository.clone(), parent_block_index)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        // Read in a scope, so the record is not held across the listing below.
        let (staged_add, dirty, grandparent) = {
            let parent = parent_block.node(parent_node_index);
            (parent.is_staged_add(), parent.is_dirty(), parent.parent)
        };

        // A parent that is itself a staged ADD is a legitimate independent
        // staged node (not merely staged to carry a descendant), so leave
        // it staged — unstaging a child must not unstage the parent.
        if stop_at_staged_add && staged_add {
            break;
        }

        // An added link's unstage takes the directory's dirty flags too, but where a dirty child
        // keeps them (C9): a file dirtied beside the link stays dirty, and so does every
        // directory above it. A directory not dirty holds no dirty child, so it is not listed.
        let flags = if stop_at_staged_add
            || (dirty
                && state_staged
                    .node_has_dirty_children(repository.clone(), parent_node_id)
                    .await
                    .forward::<UnstageError>("Failed to check node children")?)
        {
            OwnFlags::Staged
        } else {
            OwnFlags::Change
        };
        state_staged.node_clear_own(&parent_block, parent_node_id, flags);
        cleared |= parent_node_id == starting_parent;

        link_tracker.on_node_changed(&repository);

        if parent_node_id == ROOT_NODE {
            break;
        }

        parent_node_id = grandparent;
    }

    Ok(cleared)
}

/// Unstages the node `at` maps.
///
/// `states` carries the verdict this node's own query steps from, which the query asks about
/// `at`'s path from.
///
/// A staged file takes back the content of the current revision's file where
/// [`takes_back_content`] says so.
///
/// Unless `options` asks for the single node, a directory other than an add keeps its own staged
/// flags while anything below it stays staged: once its children are unstaged,
/// [`unstage_parent_chain`] runs from the directory itself, and clears them only where nothing
/// below is staged. A link is marked staged again while the linked tree holds something staged,
/// which an unstage that follows the view leaves past it.
#[allow(clippy::too_many_arguments)]
async fn unstage_node(
    operation: Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    discard: Arc<Discards>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
    states: FilterStates,
) -> Result<(), UnstageError> {
    let NodeMapping {
        repository,
        state: state_staged,
        path: node_path,
        node: node_id,
    } = at;
    let name = node_path.name();
    if name.is_empty() || name == "." {
        return Ok(());
    }

    if is_reserved_node_name(name) {
        lore_debug!("Ignore dot directory {name}");
        return Ok(());
    }

    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    let block = state_staged
        .block_with_nametable(repository.clone(), block_index)
        .await
        .forward::<UnstageError>("Failed deserializing state node block")?;
    let mut node = block.node(node_index);

    // Past the ignore rules where the staged tree tracks a change at or below the node, which it
    // is asked only where the rules would leave the node out.
    let force = execution_context().globals().force() || options.past_filter;
    let (child_states, reason) = if force {
        (FilterStates::ROOT, None)
    } else {
        repository
            .filter
            .child_tracked_reason(
                states,
                &node_path,
                !node.is_file(),
                FilterMode::TRACKED,
                async || {
                    state_staged
                        .holds_tracked_change(
                            &repository,
                            node_id,
                            NodeFlags::from_bits_retain(node.flags),
                        )
                        .await
                },
            )
            .await
            .forward::<UnstageError>("Failed to read the revision the staged tree was built on")?
    };
    if Filter::emit(&node_path, reason) {
        lore_debug!("Node excluded by filter: {}", node_path.as_str());
        return Ok(());
    }

    lore_trace!(
        "Unstage node '{name}' at path '{}' in repository {}",
        node_path.as_str(),
        repository.id
    );

    lore_debug!("Found node {node_id}");

    if !node.is_staged() && !execution_context().globals().force() {
        lore_debug!("Node {node_id} is not staged");
        return Ok(());
    }

    // Unstage clears the stage flags but preserves the dirty flag: a
    // staged ADD therefore survives as a dirty add (and a directory add
    // demotes its whole subtree likewise), rather than being discarded.
    // The staged anchor is not removed here — the end-of-unstage logic
    // removes it only when no staged AND no dirty nodes remain. A
    // staged-add LINK is still discarded so its registry entry is cleaned.
    let mut keep_as_dirty_add = false;
    let mut kept_below = false;
    if node.is_staged_add() {
        if node.is_link() {
            return discard_staged_add(
                &operation,
                NodeMapping {
                    repository,
                    state: state_staged,
                    path: node_path,
                    node: node_id,
                },
                &node,
                state_current,
                &discard,
                options,
                &stats,
                link_tracker,
            )
            .await;
        }

        lore_debug!("Unstaging staged add {node_id}: keep as dirty add");

        kept_below = node.is_directory()
            && demote_subnodes_to_dirty(
                repository.clone(),
                state_staged.clone(),
                node_path.clone(),
                node_id,
                view_below(&repository, (!force).then_some(child_states), &node_path),
                stats.clone(),
            )
            .await?;

        if !kept_below {
            state_staged.node_clear_own(&block, node_id, OwnFlags::Staged);
            node.clear_staged_flags();
        }
        link_tracker.on_node_changed(&repository);

        keep_as_dirty_add = true;
    }

    // Default values for the non-keep paths below; only read for link
    // nodes, which always go through the `!keep_as_dirty_add` branch.
    let mut was_staged_delete = false;
    let mut current_node = node;
    if !keep_as_dirty_add {
        let current_block = state_current
            .block(repository.clone(), block_index)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;

        current_node = current_block.node(node_index);

        was_staged_delete = node.is_staged_delete();

        let kept_link = node.is_link() && current_node.is_link();
        let relinked = kept_link
            && !was_staged_delete
            && current_node.linked_node().repository != node.linked_node().repository;
        let is_staged_update_link = relinked
            || link::is_staged_link_update(&node, repository.clone())
                .await
                .forward::<UnstageError>("Failed to check for a staged link change")?;

        // A staged modification of a file takes the current revision's mode and size back. A
        // staged change to a link takes back the current revision's pin, the node it exposes and
        // its registry row, or only the row's branch and flags where changes inside a link to the
        // same repository are staged. A link staged for delete takes back the pin, which a link
        // added over it and removed again replaced. The rest of the node stays as it is.
        if node.is_staged_modify() && node.is_file() {
            node.mode = current_node.mode;
            node.size = current_node.size;
        } else if is_staged_update_link {
            let at = NodeMapping {
                repository: repository.clone(),
                state: state_staged.clone(),
                path: node_path.clone(),
                node: node_id,
            };
            if relinked {
                link::reset::reset_staged_relink(
                    &operation,
                    at,
                    state_current.clone(),
                    node,
                    current_node,
                    overwrite_local(options),
                )
                .await
            } else {
                link::reset::reset_staged_update_link(
                    &operation,
                    at,
                    state_current.clone(),
                    node,
                    current_node,
                    overwrite_local(options),
                )
                .await
            }
            .forward::<UnstageError>("Failed to reset staged-update link")?;
            if relinked {
                link_tracker.remove_link(node.linked_node().repository, node_id, repository.id);
            }

            let moved_to = node.address.hash;
            take_back_link_pin(&state_staged, &block, node_id, &mut node, &current_node)?;
            stats
                .directory_unstaged_count
                .fetch_add(1, Ordering::Relaxed);
            if !relinked {
                unstage_mounts_moved_with(
                    &operation,
                    (&repository, &state_staged, node_id),
                    (node.linked_node().repository, moved_to),
                    &state_current,
                    &discard,
                    options,
                    &stats,
                    &link_tracker,
                )
                .await?;
            }
        } else if kept_link && was_staged_delete {
            take_back_link_pin(&state_staged, &block, node_id, &mut node, &current_node)?;
        } else if kept_link {
            link::reset::reset_staged_link_branching(
                repository.clone(),
                &state_staged,
                &state_current,
                node.linked_node().repository,
                node_id,
            )
            .await
            .forward::<UnstageError>("Failed to restore the link's branch and flags")?;
        }

        let restore_file = node.is_staged_modify() && node.is_file();
        let restore_content = takes_back_content(&node, &current_node)
            .forward_with::<UnstageError, _>(|| {
                format!("taking back the content of {node_path}")
            })?;
        link_tracker.on_node_changed(&repository);
        if !node.is_directory() || options.single_node {
            node.clear_staged_flags();
            let place = (node.parent, node.name_hash);
            state_staged.node_change_in_place(&block, node_id, place, |held| {
                if restore_content {
                    held.overwrite_content(
                        current_node.address,
                        current_node.size,
                        current_node.mode,
                    );
                } else if restore_file {
                    held.set_mode(current_node.mode);
                    held.set_size(current_node.size);
                }
                held.clear_staged_flags();
                true
            });
        }

        // After clearing Staged, re-check filesystem: clear Dirty if file matches current
        // revision. If still differs, preserve Dirty.
        if node.is_dirty()
            && node.is_file()
            && let Ok(info) = operation.file_info(&node_path).await
            && info.is_file()
        {
            let file_modified = crate::state::file_modification(
                repository.clone(),
                &current_node,
                info.mtime(),
                info.size(),
                &node_path,
                true, /* Force hash check */
                &operation,
                &lore_storage::ContentHashes::default(),
            )
            .await
            .forward::<UnstageError>("Failed to check if file was modified")?
            .is_modified();

            if !file_modified {
                // File matches current revision — clear Dirty
                node.clear_dirty_flags();
                // The place read again from the node, so no await above holds it.
                let place = (node.parent, node.name_hash);
                state_staged
                    .node_change_in_place(&block, node_id, place, |held| held.clear_dirty_flags());
            }
        }
        // If file doesn't exist on disk — could be a delete, preserve Dirty
    }

    unstage_parent_chain(
        repository.clone(),
        state_staged.clone(),
        node.parent,
        link_tracker.clone(),
        true,
    )
    .await?;

    if !node.is_dirty() {
        state_staged
            .node_clear(&repository, node.parent, Clear::Vacated)
            .await
            .forward::<UnstageError>("Failed to clear the dirty parents")?;
    }

    if node.is_link() {
        lore_debug!(
            "Processing link node {node_id} at path '{}' in repository {}",
            node_path.as_str(),
            repository.id
        );

        let link_metadata = node.linked_node();

        let linked_repository = repository.to_link_context(link_metadata.repository).await;

        let linked_state = link_tracker
            .track(
                (&repository, &state_staged),
                node_id,
                link_metadata.repository,
                link_metadata.revision,
            )
            .await
            .forward_with::<UnstageError, _>(|| {
                format!("Failed to fetch the link at {node_path}")
            })?;

        if was_staged_delete
            && !link_committed(
                &repository,
                &state_current,
                link_metadata.repository,
                node_id,
            )
            .await?
        {
            // A link added and replaced before it was committed has nothing to restore.
            queue_discard(&discard, &repository, &state_staged, node_id);
        } else if was_staged_delete {
            link::reset::reset_staged_remove_link(
                &operation,
                NodeMapping {
                    repository: repository.clone(),
                    state: state_staged.clone(),
                    path: node_path.clone(),
                    node: node_id,
                },
                state_current.clone(),
                current_node,
                overwrite_local(options),
            )
            .await
            .forward::<UnstageError>("Failed to reset staged-remove link")?;

            state_staged
                .node_clear(&repository, node_id, Clear::Dirty)
                .await
                .forward::<UnstageError>("Failed to clear the dirty parents")?;
            stats
                .directory_unstaged_count
                .fetch_add(1, Ordering::Relaxed);
        }

        if options.single_node {
            lore_debug!("Single node option set, skipping link directory processing");
            return Ok(());
        }

        if current_node.is_link() {
            let current_link_metadata = current_node.linked_node();

            let linked_state_current =
                State::deserialize(linked_repository.clone(), current_link_metadata.revision)
                    .await
                    .forward::<UnstageError>("Failed to unstage link nodes")?;

            unstage_directory(
                operation.clone(),
                NodeMapping {
                    repository: linked_repository.clone(),
                    state: linked_state.clone(),
                    path: node_path.clone(),
                    node: node.child,
                },
                linked_state_current.clone(),
                discard.clone(),
                options,
                stats.clone(),
                link_tracker.clone(),
                child_states,
            )
            .await?;

            if linked_state
                .node_has_staged_children(linked_repository.clone(), node.child)
                .await
                .forward::<UnstageError>("Failed to check node children")?
            {
                state_staged
                    .node_mark(&repository, node_id, Mark::staged(NodeFlags::Staged, true))
                    .await
                    .forward::<UnstageError>("Failed to mark the link staged")?;
            }
        }
    } else if node.is_directory() {
        if options.single_node {
            return Ok(());
        }

        unstage_directory(
            operation.clone(),
            NodeMapping {
                repository: repository.clone(),
                state: state_staged.clone(),
                path: node_path.clone(),
                node: node_id,
            },
            state_current.clone(),
            discard.clone(),
            options,
            stats.clone(),
            link_tracker.clone(),
            child_states,
        )
        .await?;

        let unstaged = if keep_as_dirty_add {
            !kept_below
        } else {
            !state_staged
                .node(repository.clone(), node_id)
                .await
                .forward::<UnstageError>("Failed deserializing state node block")?
                .is_staged()
                || unstage_parent_chain(
                    repository.clone(),
                    state_staged.clone(),
                    node_id,
                    link_tracker.clone(),
                    true,
                )
                .await?
        };
        if unstaged {
            stats
                .directory_unstaged_count
                .fetch_add(1, Ordering::Relaxed);
        }
    } else {
        stats.file_unstaged_count.fetch_add(1, Ordering::Relaxed);
        event::LoreEvent::FileUnstageFile(LoreFileUnstageFileEventData {
            path: LoreString::from(&node_path),
            action: LoreFileAction::Keep,
        })
        .send();
    }

    Ok(())
}

/// Unstages each other mount of `link_repository` that pins `moved_to`, the revision a staged update
/// moved the link `node_id` to, as a staged update moves every mount of a repository.
///
/// Only for the top-level repository, whose working tree paths are its paths in the tree; in a
/// linked repository the mounts are left as they are.
#[allow(clippy::too_many_arguments)]
async fn unstage_mounts_moved_with(
    operation: &Arc<InstanceOperationImpl>,
    (repository, state_staged, node_id): (&Arc<RepositoryContext>, &Arc<State>, NodeID),
    (link_repository, moved_to): (RepositoryId, Hash),
    state_current: &Arc<State>,
    discard: &Arc<Discards>,
    options: UnstageOptions,
    stats: &Arc<UnstageStats>,
    link_tracker: &Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    if repository.is_link() || repository.is_layer() {
        return Ok(());
    }
    let rows = state_staged
        .link_list(repository.clone())
        .await
        .forward::<UnstageError>("Failed reading the link registry")?;
    for row in rows
        .iter()
        .filter(|row| row.repository() == link_repository && row.local_node() != node_id)
    {
        let mount = state_staged
            .node(repository.clone(), row.local_node())
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        if !mount.is_staged() || mount.address.hash != moved_to {
            continue;
        }
        let Ok(mount_path) = RelativePath::from_str(
            &state_staged
                .node_path(repository.clone(), row.local_node())
                .await
                .forward::<UnstageError>("Failed resolving the path of a mount")?,
        );
        let states = repository.filter.parent_exclusion_states(&mount_path);
        unstage_node_recurse(
            operation.clone(),
            NodeMapping {
                repository: repository.clone(),
                state: state_staged.clone(),
                path: mount_path,
                node: row.local_node(),
            },
            state_current.clone(),
            discard.clone(),
            UnstageOptions {
                single_node: true,
                ..options
            },
            stats.clone(),
            link_tracker.clone(),
            states,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn unstage_node_recurse<'a>(
    operation: Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    discard: Arc<Discards>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
    states: FilterStates,
) -> Pin<Box<dyn Future<Output = Result<(), UnstageError>> + Send + 'a>> {
    Box::pin(unstage_node(
        operation,
        at,
        state_current,
        discard,
        options,
        stats,
        link_tracker,
        states,
    ))
}

/// Unstages the path a staged type change holds: `replaced`, the node of another kind staged for
/// delete, and the add `at` maps, which replaced it. Where `replaced` takes the path back, the add
/// is discarded; a link never committed takes nothing back, so the add stays as a dirty add.
#[allow(clippy::too_many_arguments)]
async fn unstage_replacement(
    operation: Arc<InstanceOperationImpl>,
    at: NodeMapping,
    replaced: NodeID,
    state_current: Arc<State>,
    discard: Arc<Discards>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
    states: FilterStates,
) -> Result<(), UnstageError> {
    let read = async |node_id| {
        at.state
            .node(at.repository.clone(), node_id)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")
    };
    let replaced_node = read(replaced).await?;
    let takes_path_back = !replaced_node.is_link()
        || link_committed(
            &at.repository,
            &state_current,
            replaced_node.linked_node().repository,
            replaced,
        )
        .await?;

    unstage_node_recurse(
        operation.clone(),
        NodeMapping {
            node: replaced,
            ..at.clone()
        },
        state_current.clone(),
        discard.clone(),
        options,
        stats.clone(),
        link_tracker.clone(),
        states,
    )
    .await?;

    if !takes_path_back {
        return unstage_node_recurse(
            operation,
            at,
            state_current,
            discard,
            options,
            stats,
            link_tracker,
            states,
        )
        .await;
    }
    let added = read(at.node).await?;
    discard_staged_add(
        &operation,
        at,
        &added,
        state_current,
        &discard,
        options,
        &stats,
        link_tracker,
    )
    .await
}

async fn process_link_unstage_updates(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_staged: Arc<State>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    link::drain_link_tracker(repository, token, state_staged, &link_tracker, false)
        .await
        .forward::<UnstageError>("Failed to unstage link nodes")
}

/// Puts the pin and the mounted node of `current`, the current revision's link, back on `node`, the
/// staged link at `node_id` in `block`.
fn take_back_link_pin(
    state: &State,
    block: &Arc<NodeBlock>,
    node_id: NodeID,
    node: &mut Node,
    current: &Node,
) -> Result<(), UnstageError> {
    node.address = current.address;
    node.child = current.child;
    state
        .node_set_link_target_in(
            block,
            node_id,
            LinkPin::Whole {
                address: node.address,
                child: node.child,
            },
        )
        .forward::<UnstageError>("Failed to restore the link's pin")
}

/// Whether an unstage of `node` takes back the content of `current`, the node the current
/// revision holds at the same node: where `node` is a staged file whose content hash is zero,
/// content a forced stage took from the working tree over a merge, or an empty file, and `current`
/// is a live file, its address, mode and size. A zero hash with a nonzero size and no live file
/// to take back is content no commit stored, which the unstage refuses to leave.
#[lore_macro::test_pub]
fn takes_back_content(node: &Node, current: &Node) -> Result<bool, UnstageError> {
    if !(node.is_staged() && node.is_file() && node.address.hash.is_zero()) {
        return Ok(false);
    }
    if current.is_file() && !current.is_discarded() {
        return Ok(true);
    }
    if node.size != 0 {
        return Err(UnstageError::internal(
            "A staged file holds content no commit stored, and the current revision holds no file \
             to take back",
        ));
    }
    Ok(false)
}

/// The filter states a demotion below the directory `path` asks the view with: `states`, the
/// walk's at `path`, where the view could exclude something below it, and `None` where the view
/// takes in everything below `path`, so an unstage with no view asks it nothing. `None` stays
/// `None`, as under force, which passes the view.
fn view_below(
    repository: &RepositoryContext,
    states: Option<FilterStates>,
    path: &RelativePath,
) -> Option<FilterStates> {
    states.filter(|&states| !repository.filter.view_covers_subtree(states, path))
}

/// Demote a staged-add subtree to dirty in place: clear the staged flags on every
/// descendant (preserving Dirty + action bits), so an unstaged directory add
/// survives as a tree of dirty adds rather than being discarded. Each demoted node
/// is counted as unstaged and (for files) emits a Keep unstage event, matching the
/// normal per-node unstage path. Self-contained — it walks the subtree directly and
/// never re-enters `unstage_node`.
///
/// Where `view` carries the states of the walk at `relative_path` ([`view_below`]), a descendant
/// the view excludes stays staged, as the walk leaves it, and so does every directory above it.
/// Returns whether anything below stays staged.
fn demote_subnodes_to_dirty<'a>(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node_id: NodeID,
    view: Option<FilterStates>,
    stats: Arc<UnstageStats>,
) -> Pin<Box<dyn Future<Output = Result<bool, UnstageError>> + Send + 'a>> {
    Box::pin(async move {
        let mut kept = false;
        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        let node = block.node(node_index);

        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            let child_block_index = NodeBlock::index(child_node_id);
            let child_node_index = Node::index(child_node_id);
            let child_block = state
                .block_with_nametable(repository.clone(), child_block_index)
                .await
                .forward::<UnstageError>("Failed deserializing state node block")?;
            let child_node = child_block.node(child_node_index);
            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .forward::<UnstageError>("Invalid node hierarchy in unstage walk")?;
            let next_child_sibling = child_node.sibling();

            let Some(child_name) = child_block
                .node_name_ref_or_skip(child_node_index, child_node_id)
                .forward::<UnstageError>("Failed to read node name")?
            else {
                child_node_iter = next_child_sibling;
                continue;
            };
            // Takes the name by value so its block read lock ends here, rather than reaching the
            // write below (see NodeNameLock docs).
            let child_path = relative_path.push_into_buf(child_name).freeze();
            child_node_iter = next_child_sibling;

            let mut child_states = None;
            if let Some(states) = view {
                let (states, excluded) = repository.filter.child_excludes_tree(
                    states,
                    &child_path,
                    child_node.is_directory(),
                    FilterMode::View,
                );
                if excluded {
                    kept |= child_node.is_staged();
                    continue;
                }
                child_states = Some(states);
            }

            if child_node.is_directory() {
                let child_view = view_below(&repository, child_states, &child_path);
                if demote_subnodes_to_dirty(
                    repository.clone(),
                    state.clone(),
                    child_path,
                    child_node_id,
                    child_view,
                    stats.clone(),
                )
                .await?
                {
                    kept = true;
                    continue;
                }
                stats
                    .directory_unstaged_count
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                stats.file_unstaged_count.fetch_add(1, Ordering::Relaxed);
                event::LoreEvent::FileUnstageFile(LoreFileUnstageFileEventData {
                    path: LoreString::from(&child_path),
                    action: LoreFileAction::Keep,
                })
                .send();
            }

            state.node_clear_own(&child_block, child_node_id, OwnFlags::Staged);
        }

        Ok(kept)
    })
}

/// Unstages the add `at` maps and queues it for discarding, a link's mount reset with it.
#[allow(clippy::too_many_arguments)]
async fn discard_staged_add(
    operation: &Arc<InstanceOperationImpl>,
    at: NodeMapping,
    node: &Node,
    state_current: Arc<State>,
    discard: &Discards,
    options: UnstageOptions,
    stats: &UnstageStats,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    lore_debug!("Discarding staged add {}", at.node);
    if node.is_link() {
        link::reset::reset_staged_add_link(
            operation,
            at.clone(),
            state_current,
            *node,
            overwrite_local(options),
        )
        .await
        .forward::<UnstageError>("Failed to reset staged-add link")?;
        link_tracker.remove_link(node.linked_node().repository, at.node, at.repository.id);
    }

    if node.is_directory() {
        stats
            .directory_discarded_count
            .fetch_add(1, Ordering::Relaxed);
    } else {
        stats.file_discarded_count.fetch_add(1, Ordering::Relaxed);
        event::LoreEvent::FileUnstageFile(LoreFileUnstageFileEventData {
            path: LoreString::from(&at.path),
            action: LoreFileAction::Delete,
        })
        .send();
    }

    let block_index = NodeBlock::index(at.node);
    let block = at
        .state
        .block(at.repository.clone(), block_index)
        .await
        .forward::<UnstageError>("Failed deserializing state node block")?;
    // Every change flag: the node, discarded once the unstage ends, keeps no directory above it
    // staged or dirty.
    at.state.node_clear_own(&block, at.node, OwnFlags::Change);

    queue_discard(discard, &at.repository, &at.state, at.node);

    unstage_parent_chain(at.repository, at.state, node.parent, link_tracker, false).await?;
    Ok(())
}

/// The nodes an unstage discards once it ends, by the repository and the state holding them.
#[derive(Default)]
struct Discards(parking_lot::Mutex<Vec<(crate::link::LinkedState, Vec<NodeID>)>>);

fn queue_discard(
    discard: &Discards,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    node_id: NodeID,
) {
    let mut discards = discard.0.lock();
    match discards
        .iter_mut()
        .find(|((_, holding), _)| Arc::ptr_eq(holding, state))
    {
        Some((_, nodes)) => nodes.push(node_id),
        None => discards.push(((repository.clone(), state.clone()), vec![node_id])),
    }
}

// TODO(vri): UCS-12299 - Unify codepaths to discard nodes
async fn discard_nodes(discard: &Discards) -> Result<(), UnstageError> {
    let discards = std::mem::take(&mut *discard.0.lock());
    for ((repository, state), nodes) in discards {
        lore_debug!(
            "Discarding {} nodes in repository {}",
            nodes.len(),
            repository.id
        );
        for node_id in nodes {
            state::node_discard_fused(state.clone(), repository.clone(), node_id)
                .await
                .forward::<UnstageError>("Failed to discard node")?;
        }
    }
    Ok(())
}
