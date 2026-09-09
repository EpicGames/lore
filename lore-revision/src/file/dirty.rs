// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_error_set::prelude::*;

use crate::errors::*;
use crate::file::stage::LayerRoute;
use crate::file::stage::classify_stage_path;
use crate::file::stage::is_path_under_layer_mask;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::layer;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum DirtyError {
    AddressNotFound,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    RevisionNotFound,
    WriteRequired,
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

impl crate::event::EventError for DirtyError {}

#[derive(Default)]
pub struct DirtyStats {
    pub modify_count: std::sync::atomic::AtomicU64,
    pub add_count: std::sync::atomic::AtomicU64,
    pub delete_count: std::sync::atomic::AtomicU64,
}

/// Mark files as dirty in the staged state. Action is determined by checking filesystem existence
/// and current revision state:
/// - File on disk + in revision = Modify
/// - File on disk + not in revision = Add (creates node)
/// - Not on disk + in revision = Delete (recurses for directories)
/// - Not on disk + not in revision + Dirty+Add in staged = Remove node (reverted add)
/// - Not on disk + not in revision + not in staged = Ignore
///
/// Respects ignore and view filters (same as stage).
pub async fn dirty(
    repository: Arc<RepositoryContext>,
    paths: LoreArray<LoreString>,
) -> Result<Hash, DirtyError> {
    let mut relative_paths: Vec<RelativePath> = Vec::with_capacity(paths.as_slice().len());
    for path in paths.as_slice().iter() {
        if let Ok(rp) = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        {
            relative_paths.push(rp);
        } else {
            emit_path_ignore(path.as_str()).await;
            lore_trace!("Ignoring invalid path: {path}");
        }
    }

    dirty_relative_paths(repository, relative_paths).await
}

/// Apply dirty markers for already-resolved relative paths. Skips the
/// absolute → relative conversion needed by [`dirty`]'s public API so
/// callers that already work with `RelativePath` (e.g. `commit_impl`
/// replaying tracked paths against a freshly committed revision) can hand
/// them in directly.
///
/// Wraps [`dirty_relative_paths_in`] with the instance anchor I/O.
pub(crate) async fn dirty_relative_paths(
    repository: Arc<RepositoryContext>,
    paths: Vec<RelativePath>,
) -> Result<Hash, DirtyError> {
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());
    let staged_revision = state_staged.revision();

    let layers = layer::list(repository.clone())
        .await
        .forward::<DirtyError>("Failed to list layers")?;
    let (parent_paths, layer_jobs) = route_dirty_paths(&layers, paths);

    let mask = (!layers.is_empty()).then(|| Arc::new(layer::target_paths(&layers)));

    let signature = dirty_relative_paths_in_masked(
        repository.clone(),
        state_current,
        state_staged,
        parent_paths,
        mask,
    )
    .await?;

    for (layer_index, remains) in layer_jobs {
        dirty_into_layer(repository.clone(), &layers[layer_index], &remains).await?;
    }

    // Current is never anchored as staged, and matching either input state
    // means nothing was dirtied.
    if signature != current_revision && signature != staged_revision {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<DirtyError>("Failed to serialize staged anchor")?;
    }

    Ok(signature)
}

/// Splits paths into those the parent repository owns and, per layer, the mount-relative suffixes
/// that layer owns.
///
/// Layer content is deliberately absent from the parent's tree, so a path under a mount evaluated
/// against the parent's states matches nothing and every dispatch arm in [`dirty_path`] misfires.
fn route_dirty_paths(
    layers: &[layer::Layer],
    paths: Vec<RelativePath>,
) -> (Vec<RelativePath>, Vec<(usize, Vec<RelativePath>)>) {
    if layers.is_empty() {
        return (paths, Vec::new());
    }

    let targets: Vec<&str> = layers
        .iter()
        .map(|layer| layer.target_path.as_str())
        .collect();

    let mut parent_paths = Vec::new();
    let mut remains_per_layer: Vec<Vec<RelativePath>> = vec![Vec::new(); layers.len()];

    for path in paths {
        match classify_stage_path(path.as_str(), &targets) {
            LayerRoute::Inside {
                layer_index,
                remain,
            } => remains_per_layer[layer_index].push(remain),
            LayerRoute::AncestorOf { layer_indices } => {
                parent_paths.push(path);
                for layer_index in layer_indices {
                    remains_per_layer[layer_index].push(RelativePath::new());
                }
            }
            LayerRoute::Disjoint => parent_paths.push(path),
        }
    }

    let layer_jobs = remains_per_layer
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut remains)| {
            if remains.is_empty() {
                return None;
            }
            // A mount root subsumes any suffix beneath it.
            if remains.iter().any(RelativePath::is_empty) {
                remains = vec![RelativePath::new()];
            } else {
                remains.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
                remains.dedup_by(|a, b| a.as_str() == b.as_str());
            }
            Some((index, remains))
        })
        .collect();

    (parent_paths, layer_jobs)
}

/// Each `remain` names a path below the layer's mount, which the layer draws from the same offset
/// below its source. The subtree is named by its node in each of the layer's trees, so the walk
/// carries the mount path alone and the layer's own spelling never leaves this function.
async fn dirty_into_layer(
    repository: Arc<RepositoryContext>,
    layer: &layer::Layer,
    remains: &[RelativePath],
) -> Result<(), DirtyError> {
    let mount_root = repository.require_path()?.join(&layer.target_path);
    let mount_path = RelativePath::new_from_initial_path(&layer.target_path)
        .forward_with::<DirtyError, _>(|| {
            format!("Invalid layer target path {}", layer.target_path)
        })?;
    let source_path = RelativePath::new_from_initial_path(&layer.source_path)
        .forward_with::<DirtyError, _>(|| {
            format!("Invalid layer source path {}", layer.source_path)
        })?;

    let layer_state = layer
        .deserialize_current_and_staged(repository.clone())
        .await
        .forward::<DirtyError>("Failed to deserialize layer state")?;

    let current_revision = layer_state.state_current.revision();

    let walk = DirtyWalk {
        repository: layer_state.repository.clone(),
        state_current: layer_state.state_current.clone(),
        state_staged: layer_state.state_staged.clone(),
        stats: Arc::new(DirtyStats::default()),
        mask: None,
    };
    let source_root = dirty_nodes_below(
        &walk,
        DirtyNodes {
            current: ROOT_NODE,
            staged: ROOT_NODE,
        },
        &source_path,
    )
    .await;
    let force = execution_context().globals().force();

    for remain in remains {
        let path = mount_path.join(remain.as_str());
        let absolute_path = remain.to_absolute_path(&mount_root);

        let parent_states = walk.repository.filter.parent_exclusion_states(&path);
        let (states, excluded) = walk.repository.filter.child_emit_excludes_unless_forced(
            force,
            parent_states,
            &path,
            true,
            FilterMode::Full,
        );
        if excluded {
            lore_trace!("Layer path excluded by filter: {}", path.as_str());
            continue;
        }

        dirty_path(
            &walk,
            dirty_nodes_below(&walk, source_root, remain).await,
            StagedParent::below(source_root.staged, remain.parent()),
            &path,
            &absolute_path,
            DiskState::Unknown,
            states,
        )
        .await?;
    }

    let stats = &walk.stats;
    let state_staged = &walk.state_staged;

    // A staged state never hashes equal to the committed current, so pinning an unmutated layer
    // pins a staged revision with nothing in it, which makes `commit` abort with `NothingStaged`
    // after the parent has already committed.
    if !state_staged.is_dirty() {
        lore_debug!("No dirty markers for layer at {}", layer.target_path);
        return Ok(());
    }

    state_staged.reparent_onto(current_revision);

    let token = repository
        .try_write_token()
        .expect("dirty requires write access");
    let signature = state_staged
        .serialize(layer_state.repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize layer staged revision state")?;

    if signature != layer.current && !execution_context().globals().dry_run() {
        layer::store_layer_staged(
            repository.clone(),
            token,
            layer.target_path.as_str(),
            layer.repository,
            signature,
        )
        .await
        .forward::<DirtyError>("Failed to store layer staged state")?;
    }

    lore_debug!(
        "Dirtied {} paths in layer at {}, staged {signature}",
        stats.modify_count.load(Ordering::Relaxed)
            + stats.add_count.load(Ordering::Relaxed)
            + stats.delete_count.load(Ordering::Relaxed),
        layer.target_path
    );

    Ok(())
}

/// Apply dirty markers against explicit states, reading and writing no
/// anchors. Pass a clone of `state_current` as `state_staged` when nothing is
/// staged yet. Returns `state_staged`'s own revision when no path produced a
/// marker.
pub(crate) async fn dirty_relative_paths_in(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    paths: Vec<RelativePath>,
) -> Result<Hash, DirtyError> {
    dirty_relative_paths_in_masked(repository, state_current, state_staged, paths, None).await
}

/// [`dirty_relative_paths_in`] with the layer mount subtrees the parent walk must not descend
/// into, so layer content is never enqueued as parent nodes.
async fn dirty_relative_paths_in_masked(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    paths: Vec<RelativePath>,
    mask: Option<Arc<Vec<String>>>,
) -> Result<Hash, DirtyError> {
    let current_revision = state_current.revision();
    let repository_path = repository.require_path()?.to_path_buf();

    let walk = DirtyWalk {
        repository: repository.clone(),
        state_current: state_current.clone(),
        state_staged: state_staged.clone(),
        stats: Arc::new(DirtyStats::default()),
        mask,
    };
    let root = DirtyNodes {
        current: ROOT_NODE,
        staged: ROOT_NODE,
    };
    let force = execution_context().globals().force();

    for relative_path in paths.iter() {
        let parent_states = repository.filter.parent_exclusion_states(relative_path);
        let (states, excluded) = repository.filter.child_emit_excludes_unless_forced(
            force,
            parent_states,
            relative_path,
            true,
            FilterMode::Full,
        );
        if excluded {
            lore_trace!("Path excluded by filter: {}", relative_path.as_str());
            continue;
        }

        let absolute_path = relative_path.to_absolute_path(&repository_path);
        dirty_path(
            &walk,
            dirty_nodes_below(&walk, root, relative_path).await,
            StagedParent::below(root.staged, relative_path.parent()),
            relative_path,
            &absolute_path,
            DiskState::Unknown,
            states,
        )
        .await?;
    }

    let stats = &walk.stats;
    let modify = stats.modify_count.load(Ordering::Relaxed);
    let add = stats.add_count.load(Ordering::Relaxed);
    let delete = stats.delete_count.load(Ordering::Relaxed);
    let total = modify + add + delete;

    lore_debug!("Dirtied {total} paths: {modify} modified, {add} added, {delete} deleted");

    if total == 0 {
        return Ok(state_staged.revision());
    }

    // Staged states should have no revision number
    state_staged.set_revision_number(0);
    state_staged.set_parent_self(current_revision);

    // If this is the first modification of the state (cloned from current), reset other parent
    if state_staged.revision() == current_revision {
        state_staged.set_parent_other(Hash::default());
        state_staged.set_metadata_hash(Hash::default());
    }

    let token = repository
        .try_write_token()
        .expect("dirty requires write access");
    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    Ok(signature)
}

/// What a caller already established about a path on disk, so that a scan asks once per path.
enum DiskState {
    /// Described by the listing that found it.
    Present(std::fs::Metadata),
    /// Established absent, which is why the path is being visited at all.
    Absent,
    /// Not looked at; [`dirty_path`] asks.
    Unknown,
}

/// The trees a dirty walk marks and what every step of it shares.
///
/// A walk starts where a path was resolved once, and reaches every node below from the one
/// above, so nothing after that reads a path against a tree. The paths it carries are therefore
/// the working-tree paths the filesystem and the filter answer for, whatever the tree being
/// marked spells them as — which is what a layer drawn from somewhere other than its mount
/// needs.
struct DirtyWalk {
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    stats: Arc<DirtyStats>,
    mask: Option<Arc<Vec<String>>>,
}

/// The node a path names in each tree, `INVALID_NODE` where a tree holds none.
#[derive(Clone, Copy)]
struct DirtyNodes {
    current: NodeID,
    staged: NodeID,
}

impl DirtyNodes {
    /// The child named `name` of each of these, for a walk stepping into a directory.
    ///
    /// A staged tree with nothing staged in it shares its storage with the current one, so both
    /// sides answer alike and the lookup is taken once.
    async fn child(self, walk: &DirtyWalk, name: &str) -> Self {
        let name_hash = crate::hash::hash_string(name);
        let current = subnode(
            &walk.state_current,
            &walk.repository,
            self.current,
            name_hash,
        )
        .await;
        if self.current == self.staged && Arc::ptr_eq(&walk.state_current, &walk.state_staged) {
            return DirtyNodes {
                current,
                staged: current,
            };
        }
        DirtyNodes {
            current,
            staged: subnode(&walk.state_staged, &walk.repository, self.staged, name_hash).await,
        }
    }
}

/// The child of `parent` named by `name_hash`, or `INVALID_NODE` where it holds none.
async fn subnode(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    name_hash: u64,
) -> NodeID {
    if !parent.is_valid_or_root_node_id() {
        return INVALID_NODE;
    }
    state
        .find_subnode(repository.clone(), parent, name_hash)
        .await
        .unwrap_or(INVALID_NODE)
}

/// The node `names` reaches from `base` in each tree, walking down from where a caller resolved
/// the base. An empty `names` is the base itself.
async fn dirty_nodes_below(walk: &DirtyWalk, base: DirtyNodes, names: &RelativePath) -> DirtyNodes {
    let mut nodes = base;
    let mut remaining = names.clone();
    while !remaining.is_empty() {
        let name = remaining.pop_root();
        nodes = nodes.child(walk, name).await;
    }
    nodes
}

/// Where the staged directory a path is a child of is reached from.
///
/// A walk descending a directory holds that directory's node and names nothing further. A path a
/// caller names outright has ancestors nothing has visited, so `names` reaches from the node the
/// walk starts at down to the path's parent.
#[derive(Clone, Copy)]
struct StagedParent<'a> {
    base: NodeID,
    names: Option<&'a str>,
}

impl<'a> StagedParent<'a> {
    /// The directory node a walk is descending, which the staged tree already holds.
    fn node(base: NodeID) -> Self {
        StagedParent { base, names: None }
    }

    /// The node `names` reaches from `base`, for a path a caller named outright.
    fn below(base: NodeID, names: Option<&'a str>) -> Self {
        StagedParent { base, names }
    }

    /// The directory node, creating the ones that lead to it where the staged tree lacks them.
    ///
    /// Asked for only where something is added below it, so a path that turns out to need no
    /// marker leaves no directory behind.
    async fn resolve(self, walk: &DirtyWalk) -> Result<NodeID, DirtyError> {
        match self.names.filter(|names| !names.is_empty()) {
            Some(names) => ensure_dirty_parent_dirs(walk, self.base, names).await,
            None => Ok(self.base),
        }
    }
}

/// The staged node of a directory on disk the walk is about to descend, adding it where the
/// staged tree holds none.
///
/// `None` where the trees the walk marks do not reach it. The staged tree is derived from the
/// current one, so a directory the current revision holds and the staged tree does not is one a
/// link owns, and the repository behind that link marks its own.
async fn dirty_directory_node(
    walk: &DirtyWalk,
    nodes: DirtyNodes,
    parent: StagedParent<'_>,
    in_current_revision: bool,
    relative_path: &RelativePath,
) -> Result<Option<NodeID>, DirtyError> {
    if nodes.staged.is_valid_or_root_node_id() {
        return Ok(Some(nodes.staged));
    }
    if in_current_revision {
        lore_trace!(
            "Dirty directory is not held by the staged tree, not recursed: {}",
            relative_path.as_str()
        );
        return Ok(None);
    }

    let parent = parent.resolve(walk).await?;
    dirty_add_directory(walk, parent, relative_path.name(), relative_path.as_str())
        .await
        .map(Some)
}

/// Process a single path and determine the dirty action.
///
/// `nodes` names the path in each tree and `parent` reaches the staged directory it is a child
/// of, both established by the caller. `absolute_path` is passed in rather than derived from
/// `relative_path` because the working tree is the parent's while the trees marked can be a
/// layer's.
///
/// `states` is the filter verdict the caller reached for `relative_path`, which
/// the recursions below inherit instead of folding the path again per node.
#[allow(clippy::too_many_arguments)]
async fn dirty_path(
    walk: &DirtyWalk,
    nodes: DirtyNodes,
    parent: StagedParent<'_>,
    relative_path: &RelativePath,
    absolute_path: &Path,
    disk_state: DiskState,
    states: FilterStates,
) -> Result<(), DirtyError> {
    let DirtyWalk {
        repository,
        state_staged,
        stats,
        mask,
        ..
    } = walk;

    if let Some(mask) = mask.as_deref()
        && is_path_under_layer_mask(relative_path.as_str(), mask)
    {
        lore_trace!("Path inside layer mount, not a parent path: {relative_path}");
        return Ok(());
    }

    let (exists_on_disk, is_dir) = match disk_state {
        DiskState::Present(metadata) => (true, metadata.is_dir()),
        DiskState::Absent => (false, false),
        DiskState::Unknown => {
            let metadata = lore_io::IoDriver::global().metadata(&absolute_path).await;
            let is_dir = metadata.as_ref().is_ok_and(|metadata| metadata.is_dir());
            (metadata.is_ok(), is_dir)
        }
    };

    let staged_node = nodes
        .staged
        .is_valid_or_root_node_id()
        .then_some(nodes.staged);

    let staged_pending_add = match staged_node {
        Some(node_id) => state_staged
            .node(repository.clone(), node_id)
            .await
            .is_ok_and(|node| node.is_dirty_add()),
        None => false,
    };

    // A pending add is never committed, but when the staged tree has no anchor
    // yet it shares storage with `state_current` and resolves there too; exclude
    // it so re-dirtying that node (e.g. recursing a dirtied committed parent)
    // keeps it an add rather than a modify.
    let in_current_revision = !staged_pending_add && nodes.current.is_valid_or_root_node_id();

    if exists_on_disk && is_dir {
        // A new directory is itself an add; mark it so an empty one is tracked
        // even though the child recursion finds no files to anchor it.
        let Some(staged_node) =
            dirty_directory_node(walk, nodes, parent, in_current_revision, relative_path).await?
        else {
            return Ok(());
        };
        // Directory on disk -> recurse children
        lore_trace!("Dirty directory recurse: {}", relative_path.as_str());
        dirty_directory(
            walk,
            DirtyNodes {
                current: nodes.current,
                staged: staged_node,
            },
            relative_path,
            absolute_path,
            states,
        )
        .await?;
    } else if exists_on_disk && in_current_revision {
        // File on disk + in revision -> Modify
        lore_trace!("Dirty modify: {}", relative_path.as_str());
        let node_id = staged_node.unwrap_or(nodes.current);

        state_staged
            .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyModify, true)
            .await
            .forward::<DirtyError>("Failed to mark node as dirty")?;

        stats.modify_count.fetch_add(1, Ordering::Relaxed);
    } else if exists_on_disk {
        // Skip when already tracked so a repeated dirty doesn't duplicate the node.
        if staged_node.is_none() {
            lore_trace!("Dirty add: {}", relative_path.as_str());
            let parent = parent.resolve(walk).await?;
            dirty_add(walk, parent, relative_path.name()).await?;
        }
    } else if in_current_revision {
        // Not on disk + in revision -> Delete
        lore_trace!("Dirty delete: {}", relative_path.as_str());
        dirty_delete(
            repository.clone(),
            state_staged.clone(),
            staged_node.unwrap_or(nodes.current),
            relative_path,
            stats.clone(),
            states,
        )
        .await?;
    } else if let Some(staged_node) = staged_node {
        // Not on disk + not in revision + exists in staged tree
        let node = state_staged
            .node(repository.clone(), staged_node)
            .await
            .forward::<DirtyError>("Failed to get staged node")?;
        if node.is_dirty_add() {
            lore_trace!(
                "Dirty reverted add, discarding node: {}",
                relative_path.as_str()
            );
            // Clear dirty flags so the node is no longer marked
            let block_index = NodeBlock::index(staged_node);
            let node_index = Node::index(staged_node);
            let block = state_staged
                .block(repository.clone(), block_index)
                .await
                .forward::<DirtyError>("Failed to get block")?;
            {
                let mut writer = block.write();
                writer.node(node_index).clear_all_change_flags();
                writer.mark_dirty();
            }
            state_staged.block_modified(block.clone(), block_index);
            state_staged.mark_dirty();

            // Discard the node from the tree (unlink from parent, reclaim slot)
            crate::state::node_discard_patch(
                state_staged.clone(),
                repository.clone(),
                staged_node,
                |_discarded_node_id, _flags| {},
            )
            .await
            .forward::<DirtyError>("Failed to discard reverted dirty add node")?;

            // Clean up parent dirty flags if no dirty children remain
            let parent_id = node.parent;
            if parent_id.is_valid_or_root_node_id()
                && !state_staged
                    .node_has_dirty_children(repository.clone(), parent_id)
                    .await
                    .forward::<DirtyError>("Failed to check dirty children")?
            {
                let parent_block_index = NodeBlock::index(parent_id);
                let parent_node_index = Node::index(parent_id);
                let parent_block = state_staged
                    .block(repository.clone(), parent_block_index)
                    .await
                    .forward::<DirtyError>("Failed to get parent block")?;
                {
                    let mut writer = parent_block.write();
                    writer.node(parent_node_index).clear_dirty_flags();
                    writer.mark_dirty();
                }
                state_staged.block_modified(parent_block, parent_block_index);
                state_staged.mark_dirty();
            }

            stats.delete_count.fetch_add(1, Ordering::Relaxed);
        } else {
            lore_trace!(
                "Dirty ignore (not on disk, not in revision): {}",
                relative_path.as_str()
            );
        }
    } else {
        // Not on disk + not in revision + not in staged -> Ignore
        lore_trace!(
            "Dirty ignore (not on disk, not in revision): {}",
            relative_path.as_str()
        );
    }

    Ok(())
}

/// Mark a node as Dirty+Delete, recursing into directory children.
///
/// View/ignore-filtered descendants are pruned (not marked): a dirty-delete
/// directory whose contents are excluded — e.g. a view-only path like
/// `Templates` where `/Templates/*` filters the contents but not the directory
/// node itself — must not re-mark its entire filtered subtree as deleted. The
/// directory node passed in is always marked; only its excluded children are
/// skipped. The non-emitting `excludes` is used so a large pruned subtree does
/// not produce a `FilterExclude` event per node.
async fn dirty_delete(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    stats: Arc<DirtyStats>,
    states: FilterStates,
) -> Result<(), DirtyError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<DirtyError>("Failed deserializing state node block")?;

    let node = block.node(node_index);
    if node.is_dirty_delete() {
        return Ok(());
    }

    lore_trace!("Dirty delete of node {} ({})", node_id, path.as_str());
    stats.delete_count.fetch_add(1, Ordering::Relaxed);

    state
        .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyDelete, true)
        .await
        .forward::<DirtyError>("Failed to mark node as dirty delete")?;

    // Recurse into directory children
    if node.is_directory() {
        let force = execution_context().globals().force();
        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            let child_node = state
                .node(repository.clone(), child_node_id)
                .await
                .forward::<DirtyError>("Failed deserializing state node block")?;

            let child_name = state
                .node_name_clone(repository.clone(), child_node_id)
                .await
                .forward::<DirtyError>("Failed to get child name")?;
            let child_path = path.push_into_buf(&child_name).freeze();

            let (child_states, excluded) = repository.filter.child_excludes_tree_unless_forced(
                force,
                states,
                &child_path,
                child_node.is_directory(),
                FilterMode::Full,
            );
            if !excluded {
                dirty_delete_recurse(
                    repository.clone(),
                    state.clone(),
                    child_node_id,
                    child_path,
                    stats.clone(),
                    child_states,
                )
                .await?;
            } else {
                lore_trace!(
                    "Dirty delete skipping filtered child: {}",
                    child_path.as_str()
                );
            }

            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .forward::<DirtyError>("Invalid node hierarchy in dirty delete walk")?;
            child_node_iter = child_node.sibling();
        }
    }

    Ok(())
}

fn dirty_delete_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: RelativePath,
    stats: Arc<DirtyStats>,
    states: FilterStates,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DirtyError>> + Send>> {
    Box::pin(async move { dirty_delete(repository, state, node_id, &path, stats, states).await })
}

/// Add a new file node named `name` under `parent_staged` with Dirty+Add.
async fn dirty_add(walk: &DirtyWalk, parent_staged: NodeID, name: &str) -> Result<(), DirtyError> {
    let node = Node {
        flags: NodeFlags::File.bits(),
        name_hash: crate::hash::hash_string(name),
        ..Default::default()
    };

    let node_id = walk
        .state_staged
        .node_add(walk.repository.clone(), parent_staged, node, name)
        .await
        .forward::<DirtyError>("Failed to add dirty node")?;

    // Mark with propagation so reused committed ancestors are marked up to root,
    // not left clean under a dirty child where a non-scan status walk prunes them.
    walk.state_staged
        .node_mark_dirty(walk.repository.clone(), node_id, NodeFlags::DirtyAdd, true)
        .await
        .forward::<DirtyError>("Failed to mark dirty add and propagate to parents")?;

    walk.stats.add_count.fetch_add(1, Ordering::Relaxed);

    Ok(())
}

/// Mark a new (untracked) directory node named `name` under `parent_staged` as Dirty+Add in the
/// staged tree, answering with the node it added. Mirrors `dirty_add` for the directory case so a
/// brand-new EMPTY directory is tracked even when the child recursion finds no files to anchor it.
async fn dirty_add_directory(
    walk: &DirtyWalk,
    parent_staged: NodeID,
    name: &str,
    relative_path: &str,
) -> Result<NodeID, DirtyError> {
    lore_trace!("Dirty add directory: {relative_path}");

    let node = Node {
        name_hash: crate::hash::hash_string(name),
        ..Default::default()
    };
    let new_id = walk
        .state_staged
        .node_add(walk.repository.clone(), parent_staged, node, name)
        .await
        .forward::<DirtyError>("Failed to add dirty directory node")?;

    walk.state_staged
        .node_mark_dirty(walk.repository.clone(), new_id, NodeFlags::DirtyAdd, true)
        .await
        .forward::<DirtyError>("Failed to mark dirty add directory and propagate to parents")?;

    walk.stats.add_count.fetch_add(1, Ordering::Relaxed);
    Ok(new_id)
}

/// Walk the names below `base` and create the missing directory nodes, returning the final node.
/// Existing directories are reused; missing ones are created with Dirty flag so they appear in
/// the state tree.
///
/// Reaches only where a walk starts: every node below one it has visited is a child of a node it
/// holds. The caller is responsible for checking the full path against ignore filters.
async fn ensure_dirty_parent_dirs(
    walk: &DirtyWalk,
    base: NodeID,
    parent_path: &str,
) -> Result<NodeID, DirtyError> {
    let DirtyWalk {
        repository,
        state_staged: state,
        ..
    } = walk;
    let mut current_node = base;

    for segment in parent_path.split('/').filter(|s| !s.is_empty()) {
        let name_hash = crate::hash::hash_string(segment);
        if let Ok(child_id) = state
            .find_subnode(repository.clone(), current_node, name_hash)
            .await
        {
            current_node = child_id;
        } else {
            // A directory node carries no File or Link flag. Mark it with
            // propagation so reused committed ancestors are marked up to root.
            let dir_node = Node {
                name_hash,
                ..Default::default()
            };
            let new_id = state
                .node_add(repository.clone(), current_node, dir_node, segment)
                .await
                .forward::<DirtyError>("Failed to create parent directory for dirty add")?;
            state
                .node_mark_dirty(repository.clone(), new_id, NodeFlags::Dirty, true)
                .await
                .forward::<DirtyError>("Failed to mark created parent directory dirty")?;
            current_node = new_id;
        }
    }

    Ok(current_node)
}

/// Recursively mark all children of a moved directory as dirty-moved.
/// Mirrors `mark_children_moved` in stage.rs.
async fn mark_children_dirty_moved(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent_node: NodeID,
    move_flag: NodeFlags,
) -> Result<(), crate::state::StateError> {
    fn mark_children_dirty_moved_recursive(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        parent_node: NodeID,
        move_flag: NodeFlags,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::state::StateError>> + Send>,
    > {
        Box::pin(async move {
            let children = state.node_children(repository.clone(), parent_node).await?;

            for child_id in children {
                let child_node = state.node(repository.clone(), child_id).await?;

                let child_flag = if child_node.is_dirty_add() {
                    NodeFlags::DirtyAdd
                } else {
                    move_flag
                };

                state
                    .node_mark_dirty(repository.clone(), child_id, child_flag, false)
                    .await?;

                if child_node.is_directory() {
                    mark_children_dirty_moved_recursive(
                        repository.clone(),
                        state.clone(),
                        child_id,
                        move_flag,
                    )
                    .await?;
                }
            }

            Ok(())
        })
    }

    mark_children_dirty_moved_recursive(repository, state, parent_node, move_flag).await
}

/// Recursively process a directory, marking each child as dirty based on filesystem state.
///
/// `nodes` names the directory in each tree, and every child is reached from it rather than
/// resolved from `dir_path`, which is the working-tree path the filter and the listing answer for.
///
/// `states` is the filter verdict for `dir_path`, which each child steps from.
fn dirty_directory<'a>(
    walk: &'a DirtyWalk,
    nodes: DirtyNodes,
    dir_path: &'a RelativePath,
    absolute_path: &'a std::path::Path,
    states: FilterStates,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DirtyError>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = lore_io::IoDriver::global()
            .read_dir(absolute_path)
            .await
            .map_err(|e| {
                DirtyError::internal_with_context(
                    e,
                    &format!("Failed to read directory {}", absolute_path.display()),
                )
            })?;

        let force = execution_context().globals().force();
        while let Some(entry) = entries.next().await {
            let entry = entry.map_err(|e| {
                DirtyError::internal_with_context(e, "Failed to read directory entry")
            })?;
            let name_str = crate::util::fs::entry_name(entry.file_name)
                .forward::<DirtyError>("Unusable directory entry")?;
            let child_path = dir_path.push_into_buf(&name_str).freeze();

            let (child_states, excluded) =
                walk.repository.filter.child_emit_excludes_unless_forced(
                    force,
                    states,
                    &child_path,
                    true,
                    FilterMode::Full,
                );
            if excluded {
                continue;
            }

            dirty_path(
                walk,
                nodes.child(walk, name_str.as_str()).await,
                StagedParent::node(nodes.staged),
                &child_path,
                &absolute_path.join(name_str.as_str()),
                entry
                    .metadata
                    .map_or(DiskState::Unknown, DiskState::Present),
                child_states,
            )
            .await?;
        }

        // Also check for files in the current revision that are NOT on disk (deletes)
        if nodes.current.is_valid_or_root_node_id() {
            let children = walk
                .state_current
                .node_children(walk.repository.clone(), nodes.current)
                .await
                .forward::<DirtyError>("Failed to get directory children")?;

            for &child_id in &children {
                let child_name = walk
                    .state_current
                    .node_name_clone(walk.repository.clone(), child_id)
                    .await
                    .forward::<DirtyError>("Failed to get child name")?;

                let child_path_buf = dir_path.push_into_buf(&child_name);
                let child_abs = absolute_path.join(&child_name);

                // Only process children that are NOT on disk (deletes)
                if lore_io::IoDriver::global()
                    .metadata(&child_abs)
                    .await
                    .is_err()
                {
                    let child_rel = child_path_buf.freeze();
                    // Deletes are reported whatever the filter says, so the step
                    // is taken only to carry the verdict into the recursion.
                    let (child_states, _) = walk.repository.filter.child_excludes_tree(
                        states,
                        &child_rel,
                        true,
                        FilterMode::Full,
                    );
                    // The current side is the child being enumerated, so only the staged side
                    // is looked up.
                    let child_nodes = DirtyNodes {
                        current: child_id,
                        staged: subnode(
                            &walk.state_staged,
                            &walk.repository,
                            nodes.staged,
                            crate::hash::hash_string(&child_name),
                        )
                        .await,
                    };
                    dirty_path(
                        walk,
                        child_nodes,
                        StagedParent::node(nodes.staged),
                        &child_rel,
                        &child_abs,
                        DiskState::Absent,
                        child_states,
                    )
                    .await?;
                }
            }
        }

        Ok(())
    })
}

/// Mark a file as dirty-moved. Relocates the node in the staged tree from source to destination.
/// No filesystem checks — fully caller-trusted.
/// Propagates Dirty to both source parent (child removed) and destination parent (child added).
pub async fn dirty_move(
    repository: Arc<RepositoryContext>,
    from_path: String,
    to_path: String,
) -> Result<Hash, DirtyError> {
    let from_path =
        RelativePath::new_from_user_path(repository.require_path()?, from_path.as_str())
            .forward_with::<DirtyError, _>(|| format!("Invalid path {from_path}"))?;
    let to_path = RelativePath::new_from_user_path(repository.require_path()?, to_path.as_str())
        .forward_with::<DirtyError, _>(|| format!("Invalid path {to_path}"))?;

    if from_path.as_str() == to_path.as_str() {
        return Err(DirtyError::internal("Cannot move a path to itself"));
    }

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    // Find source node (must exist)
    let from_node_link = state
        .find_node_link(repository.clone(), from_path.as_str())
        .await
        .forward_with::<DirtyError, _>(|| format!("Path {from_path} does not exist"))?;

    let from_block_index = NodeBlock::index(from_node_link.node);
    let from_node_index = Node::index(from_node_link.node);
    let from_block = state
        .block(repository.clone(), from_block_index)
        .await
        .forward::<DirtyError>("Failed deserializing state node block")?;
    let mut node = from_block.node(from_node_index);
    let old_parent = node.parent;

    // Find or create the destination parent
    let to_parent_path = to_path.parent();
    let to_parent_id = match to_parent_path {
        Some(p) if !p.is_empty() => {
            state
                .find_node_link(repository.clone(), p)
                .await
                .forward::<DirtyError>("Destination parent not found")?
                .node
        }
        _ => ROOT_NODE,
    };

    // Unlink from old parent
    if node.parent != to_parent_id {
        let parent_block_index = NodeBlock::index(node.parent);
        let parent_node_index = Node::index(node.parent);
        let parent_block = state
            .block(repository.clone(), parent_block_index)
            .await
            .forward::<DirtyError>("Failed deserializing state node block")?;
        let parent_node = parent_block.node(parent_node_index);

        if parent_node.child == from_node_link.node {
            let dirtied = {
                let mut writer = parent_block.write();
                writer.node(parent_node_index).child = node.sibling;
                writer.mark_dirty()
            };
            if dirtied {
                state.block_modified(parent_block, parent_block_index);
                state.mark_dirty();
            }
        } else {
            let mut child_id = parent_node.child().unwrap_or_default();
            let mut cycle = SiblingCycleGuard::new(node.parent);
            while let Some(sibling) = {
                let child = state
                    .node(repository.clone(), child_id)
                    .await
                    .forward::<DirtyError>("Failed deserializing state node block")?;
                child
                    .walk_step(child_id, node.parent, &mut cycle)
                    .forward::<DirtyError>("Invalid node hierarchy in dirty unlink walk")?;
                child.sibling()
            } {
                if sibling == from_node_link.node {
                    let child_block_index = NodeBlock::index(child_id);
                    let child_node_index = Node::index(child_id);
                    let child_block =
                        state
                            .block(repository.clone(), child_block_index)
                            .await
                            .forward::<DirtyError>("Failed deserializing state node block")?;
                    let dirtied = {
                        let mut writer = child_block.write();
                        writer.node(child_node_index).sibling = node.sibling;
                        writer.mark_dirty()
                    };
                    if dirtied {
                        state.block_modified(child_block, child_block_index);
                        state.mark_dirty();
                    }
                    break;
                }
                child_id = sibling;
            }
        }

        // Link into new parent's child list
        let new_parent_block_index = NodeBlock::index(to_parent_id);
        let new_parent_node_index = Node::index(to_parent_id);
        let new_parent_block = state
            .block(repository.clone(), new_parent_block_index)
            .await
            .forward::<DirtyError>("Failed deserializing state node block")?;
        let sibling_node_id = new_parent_block.node(new_parent_node_index).child;
        let dirtied = {
            let mut writer = new_parent_block.write();
            writer.node(new_parent_node_index).child = from_node_link.node;
            writer.mark_dirty()
        };
        if dirtied {
            state.block_modified(new_parent_block, new_parent_block_index);
            state.mark_dirty();
        }
        node.sibling = sibling_node_id;
        node.parent = to_parent_id;
    }

    // Update name if changed
    let to_name = to_path.name();
    let from_name = from_path.name();
    if from_name != to_name {
        node.name_hash = crate::hash::hash_string(to_name);
        from_block
            .deserialize_nametable(repository.clone())
            .await
            .forward::<DirtyError>("Failed deserializing name table")?;
        (node.name_offset, node.name_length) = from_block
            .write()
            .node_name_store(to_name, node.name_offset, node.name_length)
            .forward::<DirtyError>("Failed to store node name")?;
    }

    // Write updated node back
    let dirtied = {
        let mut writer = from_block.write();
        *writer.node(from_node_index) = node;
        writer.mark_dirty()
    };
    if dirtied {
        state.block_modified(from_block, from_block_index);
        state.mark_dirty();
    }

    // Mark the node as DirtyMove
    let dirty_move_flag = if node.is_dirty_add() {
        NodeFlags::DirtyAdd
    } else {
        NodeFlags::DirtyMove
    };
    state
        .node_mark_dirty(
            repository.clone(),
            from_node_link.node,
            dirty_move_flag,
            true,
        )
        .await
        .forward::<DirtyError>("Failed to mark node as dirty move")?;

    // If this is a directory move, recursively mark all children as dirty-moved
    if node.is_directory() {
        mark_children_dirty_moved(
            repository.clone(),
            state.clone(),
            from_node_link.node,
            dirty_move_flag,
        )
        .await
        .forward::<DirtyError>("Failed to mark children as dirty moved")?;
    }

    // Propagate dirty to source parent (child removed)
    if old_parent != to_parent_id {
        state
            .node_mark_dirty(repository.clone(), old_parent, NodeFlags::Dirty, false)
            .await
            .forward::<DirtyError>("Failed to propagate dirty to source parent")?;
    }

    // Persist
    state.set_revision_number(0);
    state.set_parent_self(current_revision);
    if state.revision() == current_revision {
        state.set_parent_other(Hash::default());
        state.set_metadata_hash(Hash::default());
    }

    let token = repository
        .try_write_token()
        .expect("dirty_move requires write access");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    if signature != current_revision {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<DirtyError>("Failed to serialize staged anchor")?;
    }

    Ok(signature)
}

/// Mark a file as dirty-copied. Creates a new destination node with Dirty+Copy.
/// Source node is unchanged. No filesystem checks — fully caller-trusted.
pub async fn dirty_copy(
    repository: Arc<RepositoryContext>,
    from_path: String,
    to_path: String,
) -> Result<Hash, DirtyError> {
    let from_path =
        RelativePath::new_from_user_path(repository.require_path()?, from_path.as_str())
            .forward_with::<DirtyError, _>(|| format!("Invalid path {from_path}"))?;
    let to_path = RelativePath::new_from_user_path(repository.require_path()?, to_path.as_str())
        .forward_with::<DirtyError, _>(|| format!("Invalid path {to_path}"))?;

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    // Verify source exists
    let _from_link = state
        .find_node_link(repository.clone(), from_path.as_str())
        .await
        .forward_with::<DirtyError, _>(|| format!("Source path {from_path} does not exist"))?;

    // Find destination parent
    let to_parent_path = to_path.parent();
    let to_name = to_path.name();
    let to_parent_id = match to_parent_path {
        Some(p) if !p.is_empty() => {
            state
                .find_node_link(repository.clone(), p)
                .await
                .forward::<DirtyError>("Destination parent not found")?
                .node
        }
        _ => ROOT_NODE,
    };

    // Create destination node with Dirty+Copy
    let node = Node {
        flags: (NodeFlags::File | NodeFlags::DirtyCopy).bits(),
        name_hash: crate::hash::hash_string(to_name),
        ..Default::default()
    };

    let _node_id = state
        .node_add(repository.clone(), to_parent_id, node, to_name)
        .await
        .forward::<DirtyError>("Failed to add copy destination node")?;

    // Propagate dirty to destination parent
    state
        .node_mark_dirty(repository.clone(), to_parent_id, NodeFlags::Dirty, false)
        .await
        .forward::<DirtyError>("Failed to propagate dirty to destination parent")?;

    // Persist
    state.set_revision_number(0);
    state.set_parent_self(current_revision);
    if state.revision() == current_revision {
        state.set_parent_other(Hash::default());
        state.set_metadata_hash(Hash::default());
    }

    let token = repository
        .try_write_token()
        .expect("dirty_copy requires write access");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    if signature != current_revision {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<DirtyError>("Failed to serialize staged anchor")?;
    }

    Ok(signature)
}
