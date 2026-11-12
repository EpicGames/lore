// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_error_set::prelude::*;

use crate::errors::*;
use crate::file::node_outside_links;
use crate::file::parent_outside_links;
use crate::file::stage::is_path_under_layer_mask;
use crate::file::stage::route_layer_paths;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::interface::LoreArray;
use crate::interface::LoreNodeType;
use crate::interface::LoreString;
use crate::layer;
use crate::link::LinkedStagedContent;
use crate::link::settle_linked_staged_state;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::lore_warn;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::repository::is_reserved_node_name;
use crate::state::Ext;
use crate::state::LinkReference;
use crate::state::Mark;
use crate::state::NameChange;
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
    /// How many paths ran through a node that is no directory, which the walk did not mark.
    through_count: std::sync::atomic::AtomicU64,
    /// What the first of them was, as [`skip_through_no_directory`] words it.
    through_first: std::sync::OnceLock<String>,
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
/// Wraps [`dirty_relative_paths_in_operation`] with the instance anchor I/O, under the one
/// filesystem operation the walk reads the working tree through and finalizes as having
/// changed nothing, the markers it leaves being state alone.
pub(crate) async fn dirty_relative_paths(
    repository: Arc<RepositoryContext>,
    paths: Vec<RelativePath>,
) -> Result<Hash, DirtyError> {
    with_operation(repository.file_system(), async |operation| {
        dirty_relative_paths_in_operation(&operation, repository, paths, None).await
    })
    .await
}

/// What a commit marks dirty again past it beside the dirty paths it replays: the moves it left
/// out, and each node the revision holds where the staged tree did, with the dirty marks it
/// carried.
pub(crate) struct CarriedMarks {
    pub moves: Vec<MovedNode>,
    pub marks: Vec<(NodeID, NodeFlags)>,
}

/// A node of kind `node_type` to mark dirty as moved again to `path`, a path from the root of the
/// tree marked.
#[lore_macro::test_pub]
pub(crate) struct MovedNode {
    pub node: NodeID,
    pub node_type: LoreNodeType,
    pub path: RelativePath,
}

/// [`dirty_relative_paths`] against `operation`, which covers the parent's whole working tree
/// and every layer and link mounted in it, so one call reads through one operation however many
/// trees it marks. For a caller that already holds one.
///
/// The moves `carried` holds are marked in the parent's tree first, as [`mark_moved_again`] marks
/// them, then its marks, each on its node as it was ([`mark_carried`]), and saved with the paths'
/// marks in one save: a path replayed first would add a node at the path a move then takes its
/// node to. The path of each move that cannot be marked again is replayed with the paths.
pub(crate) async fn dirty_relative_paths_in_operation(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    mut paths: Vec<RelativePath>,
    carried: Option<&CarriedMarks>,
) -> Result<Hash, DirtyError> {
    let moves = carried.map_or(&[][..], |carried| &carried.moves[..]);
    // The walk reaches a node by name hash, which folds case, so a path through the repository's
    // own directory is dropped here rather than resolved to a node a revision holds by that name.
    let mut index = 0;
    while index < paths.len() {
        if paths[index].as_str().split('/').any(is_reserved_node_name) {
            let path = paths.remove(index);
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignore dot directory {}", path.as_str());
        } else {
            index += 1;
        }
    }

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state_staged = match state_staged {
        Some(state_staged) => state_staged,
        None if !moves.is_empty() => staged_apart(&repository, current_revision).await?,
        None => state_current.clone(),
    };
    let staged_revision = state_staged.revision();
    let skipped = if moves.is_empty() {
        Vec::new()
    } else {
        Box::pin(mark_moved_again(
            &repository,
            &state_current,
            &state_staged,
            moves,
        ))
        .await?
    };
    let carried_marks = carried.map_or(&[][..], |carried| &carried.marks[..]);
    let marked = !carried_marks.is_empty()
        && Box::pin(mark_carried(
            &repository,
            &state_staged,
            carried_marks,
            &skipped,
        ))
        .await?;
    let changed = moves.len() > skipped.len() || marked;
    paths.extend(skipped.into_iter().map(|moved| moved.path.clone()));

    let layers = layer::list(repository.clone())
        .await
        .forward::<DirtyError>("Failed to list layers")?;
    let (parent_paths, layer_jobs) = route_layer_paths(&layers, paths);

    let mask = (!layers.is_empty()).then(|| Arc::new(layer::mount_paths(&layers)));

    let signature = dirty_relative_paths_in_masked(
        operation,
        repository.clone(),
        state_current,
        state_staged.clone(),
        parent_paths,
        mask,
    )
    .await?;
    let signature = if changed && signature == staged_revision {
        Box::pin(serialize_staged(
            &repository,
            &state_staged,
            current_revision,
        )?)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?
    } else {
        signature
    };

    for (layer_index, remains) in layer_jobs {
        dirty_into_layer(
            operation,
            repository.clone(),
            &layers[layer_index],
            &remains,
        )
        .await?;
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

/// A staged tree of its own on top of `current_revision`, for moves to be marked in: a walk that
/// shares one State for the current and the staged tree reads a node moved in it at its new path
/// in both. Boxed: only a walk with moves to mark takes it.
async fn staged_apart(
    repository: &Arc<RepositoryContext>,
    current_revision: Hash,
) -> Result<Arc<State>, DirtyError> {
    Box::pin(State::deserialize(repository.clone(), current_revision))
        .await
        .forward::<DirtyError>("Failed to deserialize revision state")
}

/// Each `remain` names a path below the layer's mount, which the layer draws from the same offset
/// below its source. The subtree is named by its node in each of the layer's trees, so the walk
/// carries the mount path alone and the layer's own spelling never leaves this function.
async fn dirty_into_layer(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    layer: &layer::Layer,
    remains: &[RelativePath],
) -> Result<(), DirtyError> {
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

    let walk = DirtyWalk::new(
        operation.clone(),
        layer_state.repository.clone(),
        layer_state.state_current.clone(),
        layer_state.state_staged.clone(),
        Arc::new(DirtyStats::default()),
        None,
    )
    .await?;
    let source_root = dirty_nodes_below(
        &walk,
        DirtyNodes {
            current: ROOT_NODE,
            staged: ROOT_NODE,
        },
        &source_path,
    )
    .await;

    for remain in remains {
        let path = mount_path.join(remain.as_str());
        dirty_named_path(&walk, source_root, remain, &path).await?;
    }
    repin_dirty_links(&walk).await?;

    let stats = &walk.stats;
    let state_staged = &walk.state_staged;
    warn_through_no_directory(stats);

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
///
/// Reads the working tree through one filesystem operation, finalized as having changed
/// nothing, the markers it leaves being state alone.
#[cfg(feature = "test-util")]
#[lore_macro::test_pub]
pub(crate) async fn dirty_relative_paths_in(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    paths: Vec<RelativePath>,
) -> Result<Hash, DirtyError> {
    with_operation(repository.file_system(), async |operation| {
        dirty_relative_paths_in_masked(
            &operation,
            repository,
            state_current,
            state_staged,
            paths,
            None,
        )
        .await
    })
    .await
}

/// [`dirty_relative_paths_in`] with the layer mount subtrees the parent walk must not descend
/// into, so layer content is never enqueued as parent nodes, against `operation`, the one the
/// call reads the working tree through.
async fn dirty_relative_paths_in_masked(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    paths: Vec<RelativePath>,
    mask: Option<Arc<Vec<String>>>,
) -> Result<Hash, DirtyError> {
    dirty_drawn_paths_in(
        operation,
        repository,
        state_current,
        state_staged,
        &RelativePath::default(),
        &paths
            .into_iter()
            .map(|path| (path.clone(), path))
            .collect::<Vec<_>>(),
        mask,
        false,
    )
    .await
}

/// [`dirty_relative_paths_in_masked`] for a tree drawn from `source_path`, each of `paths` pairing
/// the names below `source_path` with the working-tree path that holds them.
///
/// `compare` marks a file the working tree and the current revision both hold only where its
/// content differs, for a caller whose recorded modified times speak for the current revision.
/// A path running through a node that is no directory is left unmarked, with one warning
/// ([`warn_through_no_directory`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dirty_drawn_paths_in(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    source_path: &RelativePath,
    paths: &[(RelativePath, RelativePath)],
    mask: Option<Arc<Vec<String>>>,
    compare: bool,
) -> Result<Hash, DirtyError> {
    let current_revision = state_current.revision();

    let mut walk = DirtyWalk::new(
        operation.clone(),
        repository.clone(),
        state_current,
        state_staged,
        Arc::new(DirtyStats::default()),
        mask,
    )
    .await?;
    walk.compare = compare;
    let root = DirtyNodes {
        current: ROOT_NODE,
        staged: ROOT_NODE,
    };
    let source_root = dirty_nodes_below(&walk, root, source_path).await;

    for (names, path) in paths {
        dirty_named_path(&walk, source_root, names, path).await?;
    }
    repin_dirty_links(&walk).await?;

    let state_staged = &walk.state_staged;
    let stats = &walk.stats;
    warn_through_no_directory(stats);
    let modify = stats.modify_count.load(Ordering::Relaxed);
    let add = stats.add_count.load(Ordering::Relaxed);
    let delete = stats.delete_count.load(Ordering::Relaxed);
    let total = modify + add + delete;

    lore_debug!("Dirtied {total} paths: {modify} modified, {add} added, {delete} deleted");

    if total == 0 {
        return Ok(state_staged.revision());
    }

    state_staged.reparent_onto(current_revision);

    let token = repository
        .try_write_token()
        .expect("dirty requires write access");
    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    Ok(signature)
}

/// The trees a dirty walk marks and what every step of it shares.
///
/// A walk starts where a path was resolved once, and reaches every node below from the one
/// above, so nothing after that reads a path against a tree. The paths it carries are therefore
/// the working-tree paths the filesystem and the filter answer for, whatever the tree being
/// marked spells them as — which is what a layer drawn from somewhere other than its mount
/// needs.
#[lore_macro::test_pub]
struct DirtyWalk {
    /// The operation the whole call reads the working tree through. A layer's or link's walk
    /// marks its own trees while its paths and its filesystem are the parent's, so this is the
    /// parent's operation whatever `repository` names.
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    stats: Arc<DirtyStats>,
    mask: Option<Arc<Vec<String>>>,
    link_references: Vec<LinkReference>,
    linked: parking_lot::Mutex<Vec<(LinkReference, Arc<DirtyWalk>)>>,
    /// Whether a file both the working tree and the current revision hold is marked only where it
    /// differs.
    compare: bool,
}

impl DirtyWalk {
    #[lore_macro::test_pub]
    async fn new(
        operation: Arc<InstanceOperationImpl>,
        repository: Arc<RepositoryContext>,
        state_current: Arc<State>,
        state_staged: Arc<State>,
        stats: Arc<DirtyStats>,
        mask: Option<Arc<Vec<String>>>,
    ) -> Result<Self, DirtyError> {
        let link_references = state_staged
            .link_list(repository.clone())
            .await
            .forward::<DirtyError>("Failed to read link registry")?;
        Ok(DirtyWalk {
            operation,
            repository,
            state_current,
            state_staged,
            stats,
            mask,
            link_references,
            linked: Default::default(),
            compare: false,
        })
    }

    fn link_reference(&self, node: NodeID) -> Option<&LinkReference> {
        self.link_references
            .iter()
            .find(|reference| reference.local_node() == node)
    }
}

/// The node a path names in each tree, `INVALID_NODE` where a tree holds none.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct DirtyNodes {
    current: NodeID,
    staged: NodeID,
}

impl DirtyNodes {
    /// The child `name_hash` names in each of these, for a walk stepping into a directory.
    /// Named by hash because a listing carries one per entry.
    ///
    /// A staged tree with nothing staged in it shares its storage with the current one, so both
    /// sides answer alike and the lookup is taken once.
    async fn child(self, walk: &DirtyWalk, name_hash: u64) -> Self {
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
        let name_hash = crate::hash::hash_string(remaining.pop_root());
        nodes = nodes.child(walk, name_hash).await;
    }
    nodes
}

async fn staged_node(walk: &DirtyWalk, nodes: DirtyNodes) -> Option<Node> {
    if !nodes.staged.is_valid_or_root_node_id() {
        return None;
    }
    walk.state_staged
        .node(walk.repository.clone(), nodes.staged)
        .await
        .ok()
}

/// Marks `path`, which `names` reaches from `base`.
///
/// The filter is asked about the path once it is resolved, entering each link on the way: as what
/// the working tree holds there, else as the node the staged tree holds, a link as a directory, and
/// as a directory where neither holds anything. A path the trees track is past the ignore slot. A
/// path the working tree lacks is asked about as a deletion, which asks the view where the walk is
/// forced too ([`FilterMode::absence`]). A path the filter leaves out whatever it holds is answered
/// before the working tree is read where the trees do not track it. A path the operation cannot
/// describe is an error and not an absent path: reading it as absent would mark a file that is
/// there as deleted.
#[lore_macro::test_pub]
async fn dirty_named_path(
    walk: &DirtyWalk,
    base: DirtyNodes,
    names: &RelativePath,
    path: &RelativePath,
) -> Result<(), DirtyError> {
    let filter = &walk.repository.filter;
    let force = execution_context().globals().force();
    let parent = filter.parent_exclusion_states(path);
    let pruned = !force && filter.excludes_any_kind(parent, path, FilterMode::Full);
    dirty_resolved_path(walk, base, names, path, parent, force, pruned).await
}

/// [`dirty_named_path`] past the answer its parent gives, `parent` being the filter verdict at the
/// parent, `force` whether the walk is forced past the filter, and `pruned` whether the filter
/// leaves out the path whatever it holds.
async fn dirty_resolved_path(
    walk: &DirtyWalk,
    base: DirtyNodes,
    names: &RelativePath,
    path: &RelativePath,
    parent: FilterStates,
    force: bool,
    pruned: bool,
) -> Result<(), DirtyError> {
    let mut nodes = base;
    let mut remaining = names.clone();
    while !remaining.is_empty() {
        let name_hash = crate::hash::hash_string(remaining.pop_root());
        nodes = nodes.child(walk, name_hash).await;
        if !remaining.is_empty()
            && let Some(&reference) = walk.link_reference(nodes.staged)
            && let Some(link) = staged_node(walk, nodes).await
        {
            let (linked, base) = linked_walk(walk, reference, link.linked_node()).await?;
            return Box::pin(dirty_resolved_path(
                &linked, base, &remaining, path, parent, force, pruned,
            ))
            .await;
        }
    }

    if pruned && !tracks(walk, nodes, None).await? {
        walk.repository
            .filter
            .emit_excludes_any_kind(parent, path, FilterMode::Full);
        lore_trace!("Path excluded by filter: {}", path.as_str());
        return Ok(());
    }

    // A block of its own, so the future marking the path does not hold the staged node beside it.
    let marking = {
        let info = walk
            .operation
            .file_info(path)
            .await
            .forward_any::<DirtyError>("Failed to describe a path in the working tree")?;
        let staged = staged_node(walk, nodes).await;
        let is_directory = if info.exists() {
            info.is_dir()
        } else {
            Node::asked_as_directory(staged.as_ref())
        };
        let filter = &walk.repository.filter;
        let tracked = !force
            && filter
                .child_excludes_tree(parent, path, is_directory, FilterMode::Full)
                .1
            && tracks(walk, nodes, Some(is_directory)).await?;
        let mode = FilterMode::TRACKED.at(tracked);
        let (states, excluded) = if info.exists() {
            filter.child_emit_unless_forced(
                force,
                FilterMode::View,
                parent,
                path,
                is_directory,
                mode,
            )
        } else {
            filter.child_emit_excludes(parent, path, is_directory, mode.absence(force))
        };
        if excluded {
            lore_trace!("Path excluded by filter: {}", path.as_str());
            return Ok(());
        }
        dirty_path(
            walk,
            nodes,
            staged,
            StagedParent::below(base.staged, names.parent()),
            path,
            info,
            states,
        )
    };
    marking.await
}

/// Whether the trees track what `nodes` names as a directory where `is_directory` says, or as either
/// kind where it is `None`: the current revision tracks all it holds, and the staged tree what it
/// stages or holds from the revision it was built on, wherever it has since moved it
/// ([`State::tracks`]). An add or a copy only marked dirty is not tracked.
async fn tracks(
    walk: &DirtyWalk,
    nodes: DirtyNodes,
    is_directory: Option<bool>,
) -> Result<bool, DirtyError> {
    let as_asked = |directory: bool| is_directory.is_none_or(|asked| asked == directory);
    if nodes.current.is_valid_node_id() {
        if is_directory.is_none() {
            return Ok(true);
        }
        if let Ok(current) = walk
            .state_current
            .node(walk.repository.clone(), nodes.current)
            .await
            && as_asked(Node::asked_as_directory(Some(&current)))
        {
            return Ok(true);
        }
    }
    let Some(staged) = staged_node(walk, nodes).await else {
        return Ok(false);
    };
    let flags = NodeFlags::from_bits_retain(staged.flags);
    if !as_asked(!flags.contains(NodeFlags::File)) {
        return Ok(false);
    }
    walk.state_staged
        .tracks(&walk.repository, nodes.staged, flags)
        .await
        .forward::<DirtyError>("Failed to read the revision the staged tree was built on")
}

/// The walk marking the trees of the link `reference` names, pinned at `pin`, opened once per
/// call, and the nodes it starts from.
async fn linked_walk(
    walk: &DirtyWalk,
    reference: LinkReference,
    pin: NodeLink,
) -> Result<(Arc<DirtyWalk>, DirtyNodes), DirtyError> {
    let cached = walk
        .linked
        .lock()
        .iter()
        .find(|(linked, _)| linked.local_node() == reference.local_node())
        .map(|(_, linked)| linked.clone());
    let linked = if let Some(linked) = cached {
        linked
    } else {
        let (repository, state_staged) = pin
            .open(&walk.repository)
            .await
            .forward::<DirtyError>("Failed to deserialize linked staged state")?;
        let committed = state_staged.committed_revision();
        let state_current = if committed == pin.revision {
            state_staged.clone()
        } else {
            State::deserialize(repository.clone(), committed)
                .await
                .forward::<DirtyError>("Failed to deserialize linked state")?
        };
        let mut linked = DirtyWalk::new(
            walk.operation.clone(),
            repository,
            state_current,
            state_staged,
            walk.stats.clone(),
            None,
        )
        .await?;
        linked.compare = walk.compare;
        let linked = Arc::new(linked);
        walk.linked.lock().push((reference, linked.clone()));
        linked
    };

    let base = DirtyNodes {
        current: pin.node,
        staged: pin.node,
    };
    Ok((linked, base))
}

/// Pin each link the walk marked inside to its linked staged state, innermost first.
async fn repin_dirty_links(walk: &DirtyWalk) -> Result<(), DirtyError> {
    let linked = std::mem::take(&mut *walk.linked.lock());
    if linked.is_empty() {
        return Ok(());
    }
    let token = walk
        .repository
        .try_write_token()
        .expect("dirty requires write access");
    for (reference, linked) in linked {
        Box::pin(repin_dirty_links(&linked)).await?;

        if !linked.state_staged.is_dirty() {
            continue;
        }

        // A walk only adds markers, so a state it mutated holds one.
        settle_linked_staged_state(
            &walk.state_staged,
            walk.repository.clone(),
            reference.local_node(),
            reference.branch,
            &(linked.repository.clone(), linked.state_staged.clone()),
            LinkedStagedContent::Dirty,
            token,
        )
        .await
        .forward::<DirtyError>("Failed to update link pin")?;
    }
    Ok(())
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

    /// The directory node, creating the ones that lead to it where the staged tree lacks them, or
    /// the node that is no directory they would lead through ([`ParentChain::Through`]).
    ///
    /// Asked for only where something is added below it, so a path that turns out to need no
    /// marker leaves no directory behind.
    async fn resolve(self, walk: &DirtyWalk) -> Result<ParentChain, DirtyError> {
        match self.names.filter(|names| !names.is_empty()) {
            Some(names) => {
                ensure_dirty_parent_dirs(&walk.repository, &walk.state_staged, self.base, names)
                    .await
            }
            None => Ok(ParentChain::Reached(self.base)),
        }
    }
}

/// Counts `path`, a working-tree path, as a path the walk leaves unmarked, as it runs through a
/// node of the staged tree that is no directory, with `below` of its names below that node
/// ([`ParentChain::Through`]), and words the first such path, naming both by their working-tree
/// paths, for the one warning the walk ends with ([`warn_through_no_directory`]).
fn skip_through_no_directory(walk: &DirtyWalk, path: &RelativePath, below: usize) {
    if walk.stats.through_count.fetch_add(1, Ordering::Relaxed) > 0 {
        return;
    }
    let at = path
        .as_str()
        .rsplitn(below + 1, '/')
        .last()
        .unwrap_or_default();
    let _ = walk
        .stats
        .through_first
        .set(format!("Cannot mark {path} dirty: {at} is not a directory"));
}

/// Warns once of the paths a walk left unmarked, as each runs through a node that is no
/// directory, naming the first and counting the rest ([`skip_through_no_directory`]).
fn warn_through_no_directory(stats: &DirtyStats) {
    let skipped = stats.through_count.load(Ordering::Relaxed);
    match (skipped, stats.through_first.get()) {
        (1, Some(first)) => lore_warn!("{first}"),
        (2, Some(first)) => {
            lore_warn!("{first}; 1 more path through a node that is not a directory is not marked");
        }
        (_, Some(first)) => lore_warn!(
            "{first}; {} more paths through a node that is not a directory are not marked",
            skipped - 1
        ),
        _ => {}
    }
}

/// The staged node of a directory on disk the walk is about to descend, adding it where the
/// staged tree holds none.
///
/// `None` where the trees the walk marks do not reach it. The staged tree is derived from the
/// current one, so a directory the current revision holds and the staged tree does not was
/// dropped from it.
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

    let parent = match parent.resolve(walk).await? {
        ParentChain::Reached(parent) => parent,
        ParentChain::Through(below) => {
            skip_through_no_directory(walk, relative_path, below + 1);
            return Ok(None);
        }
    };
    dirty_add_directory(walk, parent, relative_path.name(), relative_path.as_str())
        .await
        .map(Some)
}

/// Process a single path and determine the dirty action.
///
/// `nodes` names the path in each tree and `parent` reaches the staged directory it is a child
/// of, both established by the caller. `relative_path` is the working tree's path, which the
/// walk's operation answers for, while the trees marked can be a layer's.
///
/// `staged` is the node `nodes` names in the staged tree, and `info` what the working tree holds at
/// `relative_path`, as the caller read them. A file the staged tree marks moved and not staged is
/// marked modified as well, keeping its move ([`content_change`]).
///
/// `states` is the filter verdict the caller reached for `relative_path`, which
/// the recursions below inherit instead of folding the path again per node.
#[allow(clippy::too_many_arguments)]
async fn dirty_path(
    walk: &DirtyWalk,
    nodes: DirtyNodes,
    staged: Option<Node>,
    parent: StagedParent<'_>,
    relative_path: &RelativePath,
    info: FileInfo,
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

    let (exists_on_disk, is_dir) = (info.exists(), info.is_dir());

    if let Some(&reference) = walk.link_reference(nodes.staged)
        && let Some(link) = staged
    {
        if !exists_on_disk {
            lore_trace!("Link mount absent from the working tree, not marked: {relative_path}");
            return Ok(());
        }
        let (linked, base) = linked_walk(walk, reference, link.linked_node()).await?;
        return Box::pin(dirty_path(
            &linked,
            base,
            staged_node(&linked, base).await,
            StagedParent::node(base.staged),
            relative_path,
            info,
            states,
        ))
        .await;
    }

    let nodes = apart_from_moved(nodes, staged.is_some_and(|node| node.is_dirty_move()));
    let staged_node = nodes
        .staged
        .is_valid_or_root_node_id()
        .then_some(nodes.staged);
    let staged_pending_add = staged.is_some_and(|node| node.is_dirty_add());

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
            states,
        )
        .await?;
    } else if exists_on_disk && in_current_revision {
        let Some(action) = content_change(staged) else {
            lore_trace!("Dirty move kept: {}", relative_path.as_str());
            return Ok(());
        };
        if walk.compare && file_matches_current(walk, nodes.current, relative_path, &info).await? {
            return Ok(());
        }

        // File on disk + in revision -> Modify
        lore_trace!("Dirty modify: {}", relative_path.as_str());
        mark_modified(walk, staged_node.unwrap_or(nodes.current), action).await?;
    } else if exists_on_disk {
        // Skip when already tracked so a repeated dirty doesn't duplicate the node.
        if let Some(staged_node) = staged_node {
            if let Some(moved) = staged.filter(|node| node.is_file() && !node.is_staged())
                && moved.is_dirty_move()
            {
                lore_trace!("Dirty modify of a move: {}", relative_path.as_str());
                mark_modified(
                    walk,
                    staged_node,
                    NodeFlags::DirtyModify | NodeFlags::DirtyMove,
                )
                .await?;
            }
        } else {
            lore_trace!("Dirty add: {}", relative_path.as_str());
            match parent.resolve(walk).await? {
                ParentChain::Reached(parent) => {
                    dirty_add(walk, parent, relative_path.name()).await?;
                }
                ParentChain::Through(below) => {
                    skip_through_no_directory(walk, relative_path, below + 1);
                }
            }
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
        // Not on disk + not in revision + exists in staged tree. The flag and the parent alone, so
        // the record is not held across the awaits below.
        let (dirty_add, parent) = state_staged
            .node(repository.clone(), staged_node)
            .await
            .forward::<DirtyError>("Failed to get staged node")
            .map(|node| (node.is_dirty_add(), node.parent))?;
        if dirty_add {
            lore_trace!(
                "Dirty reverted add, discarding node: {}",
                relative_path.as_str()
            );
            crate::state::node_discard_fused(state_staged.clone(), repository.clone(), staged_node)
                .await
                .forward::<DirtyError>("Failed to discard reverted dirty add node")?;

            crate::state::clear_propagation(state_staged, repository, parent)
                .await
                .forward::<DirtyError>("Failed to clear the marks above a reverted add")?;

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

/// The dirty action that marks a change to the content of the file the staged tree holds as
/// `staged`: modified, and moved as well where it is marked moved, the move standing beside the
/// edit. `None` for a move that is staged, whose staged action the dirty mark shares and would
/// replace.
fn content_change(staged: Option<Node>) -> Option<NodeFlags> {
    match staged {
        Some(node) if node.is_dirty_move() && node.is_staged() => None,
        Some(node) if node.is_dirty_move() => Some(NodeFlags::DirtyModify | NodeFlags::DirtyMove),
        _ => Some(NodeFlags::DirtyModify),
    }
}

/// Marks `node` dirty with `action`, a change to its content, counting it as a modification.
async fn mark_modified(
    walk: &DirtyWalk,
    node: NodeID,
    action: NodeFlags,
) -> Result<(), DirtyError> {
    walk.state_staged
        .node_mark(
            &walk.repository,
            node,
            Mark::dirty(action, Ext::Set(NodeExtFlags::NoFlags), true),
        )
        .await
        .forward::<DirtyError>("Failed to mark node as dirty")?;
    walk.stats.modify_count.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// `nodes` with no node of the current revision where the staged tree holds a node marked dirty as
/// moved there (`moved`) and the revision holds another: the moved node carries its change, as it
/// does where the revision holds nothing at its new path, and the other node is no part of it.
fn apart_from_moved(nodes: DirtyNodes, moved: bool) -> DirtyNodes {
    if moved && nodes.current != nodes.staged {
        DirtyNodes {
            current: INVALID_NODE,
            staged: nodes.staged,
        }
    } else {
        nodes
    }
}

/// Whether the file at `path`, which `info` describes, holds what the current revision's node
/// `current` holds, as `status --scan` would answer.
///
/// The comparison is boxed: only a walk that compares takes it, and inline its future would be
/// held by every step of every walk.
async fn file_matches_current(
    walk: &DirtyWalk,
    current: NodeID,
    path: &RelativePath,
    info: &FileInfo,
) -> Result<bool, DirtyError> {
    let node = walk
        .state_current
        .node(walk.repository.clone(), current)
        .await
        .forward::<DirtyError>("Failed to get current node")?;
    if !node.is_file() || info.mode_differs_from(node.mode) {
        return Ok(false);
    }
    let modification = Box::pin(crate::state::file_modified_against_node(
        walk.repository.clone(),
        &node,
        info.mtime(),
        info.size(),
        path,
        true,
        &walk.operation,
        &lore_storage::ContentHashes::default(),
    ))
    .await
    .forward::<DirtyError>("Failed to compare a file to the current revision")?;
    Ok(!modification.is_modified())
}

/// Mark a node as Dirty+Delete, recursing into directory children.
///
/// View-filtered descendants are pruned (not marked): a dirty-delete
/// directory whose contents are excluded — e.g. a view-only path like
/// `Templates` where `/Templates/*` filters the contents but not the directory
/// node itself — must not re-mark its entire filtered subtree as deleted. The
/// directory node passed in is always marked; only the children the view
/// excludes are skipped, forced or not: the working tree lacks what the view
/// excludes without its having been deleted. The non-emitting
/// `child_excludes_tree` is used so a large pruned subtree does not produce a
/// `FilterExclude` event per node. The ignore slot is not asked: every
/// descendant is a node the tree holds, which is past it.
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
        .node_mark(
            &repository,
            node_id,
            Mark::dirty(
                NodeFlags::DirtyDelete,
                Ext::Set(NodeExtFlags::NoFlags),
                true,
            ),
        )
        .await
        .forward::<DirtyError>("Failed to mark node as dirty delete")?;

    // Recurse into directory children
    if node.is_directory() {
        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            let child_node = state
                .node(repository.clone(), child_node_id)
                .await
                .forward::<DirtyError>("Failed deserializing state node block")?;
            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .forward::<DirtyError>("Invalid node hierarchy in dirty delete walk")?;
            child_node_iter = child_node.sibling();

            let Some(child_name) = state
                .node_name_clone_or_skip(repository.clone(), child_node_id)
                .await
                .forward::<DirtyError>("Failed to get child name")?
            else {
                continue;
            };
            let child_path = path.push_into_buf(&child_name).freeze();

            let (child_states, excluded) = repository.filter.child_excludes_tree(
                states,
                &child_path,
                child_node.is_directory(),
                FilterMode::View,
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
        .node_mark(
            &walk.repository,
            node_id,
            Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), true),
        )
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
        .node_mark(
            &walk.repository,
            new_id,
            Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), true),
        )
        .await
        .forward::<DirtyError>("Failed to mark dirty add directory and propagate to parents")?;

    walk.stats.add_count.fetch_add(1, Ordering::Relaxed);
    Ok(new_id)
}

/// Walk the names below `base` and create the missing directory nodes, answering the final node.
/// Existing directories are reused; missing ones are created as dirty adds, as a scan creates
/// them ([`ensure_scan_dir_chain`](crate::state::os_diff::ensure_scan_dir_chain)), so they appear
/// in the state tree as the untracked content they are and the ignore rules still apply to them.
///
/// Reaches only where a walk starts: every node below one it has visited is a child of a node it
/// holds. The caller is responsible for checking the full path against ignore filters. A find
/// that fails other than for a missing name returns its error, adding nothing for that name.
///
/// The scan's chain walks the same way but adds each directory with `DirtyAdd` in its record,
/// marks only the first one's parent, and checks every name it adds before adding any; this marks
/// each directory `DirtyAdd` after its add, climbing from it, and leaves the reserved names to the
/// walk's entry and the other checks to the add.
///
/// Stops, adding nothing, at a node that is no directory, where the first name missing would be
/// added below it, as on a path through a file, which holds no child to find, or where the path
/// ends there ([`ParentChain::Through`]).
#[lore_macro::test_pub]
async fn ensure_dirty_parent_dirs(
    repository: &Arc<RepositoryContext>,
    state: &State,
    base: NodeID,
    parent_path: &str,
) -> Result<ParentChain, DirtyError> {
    let mut current_node = base;
    let mut added = false;

    // Split by hand: a `split('/')` iterator held across the awaits below costs this future 56
    // bytes more than the rest of the path does.
    let mut rest = parent_path;
    while !rest.is_empty() {
        let (segment, tail) = rest.split_once('/').unwrap_or((rest, ""));
        rest = tail;
        if segment.is_empty() {
            continue;
        }
        let name_hash = crate::hash::hash_string(segment);
        // Only a name the directory lacks is added: a find that failed otherwise, as on a block
        // that did not load, says nothing of the name, and the add checks none.
        match state
            .find_subnode(repository.clone(), current_node, name_hash)
            .await
        {
            Ok(child_id) => {
                current_node = child_id;
                continue;
            }
            Err(error) if !error.is_node_not_found() => {
                return Err(error)
                    .forward::<DirtyError>("finding a parent directory of a dirty add");
            }
            Err(_) => {}
        }
        if !added
            && !state
                .node(repository.clone(), current_node)
                .await
                .forward::<DirtyError>("Failed to read a parent directory of a dirty add")?
                .is_directory()
        {
            return Ok(ParentChain::Through(names_in(rest) + 1));
        }
        added = true;
        // A directory node carries no File or Link flag. Mark it with
        // propagation so reused committed ancestors are marked up to root.
        // A dirty add, not a bare dirty mark: the tree does not track a
        // directory it creates here.
        let dir_node = Node {
            name_hash,
            ..Default::default()
        };
        let new_id = state
            .node_add(repository.clone(), current_node, dir_node, segment)
            .await
            .forward::<DirtyError>("Failed to create parent directory for dirty add")?;
        state
            .node_mark(
                repository,
                new_id,
                Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), true),
            )
            .await
            .forward::<DirtyError>("Failed to mark created parent directory dirty")?;
        current_node = new_id;
    }

    if !added
        && !state
            .node(repository.clone(), current_node)
            .await
            .forward::<DirtyError>("Failed to read the parent directory of a dirty add")?
            .is_directory()
    {
        return Ok(ParentChain::Through(0));
    }
    Ok(ParentChain::Reached(current_node))
}

/// How many names `path` holds.
fn names_in(path: &str) -> usize {
    path.split('/').filter(|name| !name.is_empty()).count()
}

/// Where [`ensure_dirty_parent_dirs`] took a path.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParentChain {
    /// To the directory node its last name reaches.
    Reached(NodeID),
    /// To a node that is no directory, below which its first missing name would be added, or which
    /// its last name reaches, with how many of its names lie below that node.
    Through(usize),
}

/// The dirty mark of a move of `node`: an add where it is added, and otherwise a move, beside the
/// modification of a file marked modified and not staged.
fn dirty_move_mark(node: &Node) -> NodeFlags {
    if node.is_added() {
        NodeFlags::DirtyAdd
    } else if node.is_file() && node.is_dirty_modify() && !node.is_staged() {
        NodeFlags::DirtyMove | NodeFlags::DirtyModify
    } else {
        NodeFlags::DirtyMove
    }
}

/// Marks every node below the moved directory `parent_node` dirty as moved, by the rules of
/// `mark_children_moved` in `file/stage.rs`: a node marked added stays an addition, a node deleted
/// is left as it is, unless the moved directory was deleted itself (`undelete`). Any other node is
/// marked moved below an added directory too, as only a move of its own put it there, so a commit
/// returns it rather than discarding it with the directory; a file marked modified stays modified
/// as well ([`dirty_move_mark`]).
async fn mark_children_dirty_moved(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent_node: NodeID,
    undelete: bool,
) -> Result<(), crate::state::StateError> {
    fn mark_children_dirty_moved_recursive(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        parent_node: NodeID,
        undelete: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::state::StateError>> + Send>,
    > {
        Box::pin(async move {
            let children = state.node_children(repository.clone(), parent_node).await?;

            for child_id in children {
                let (deleted, child_flag, is_directory) = {
                    let child = state.node(repository.clone(), child_id).await?;
                    (
                        child.is_deleted(),
                        dirty_move_mark(&child),
                        child.is_directory(),
                    )
                };
                if !undelete && deleted {
                    continue;
                }

                state
                    .node_mark(
                        &repository,
                        child_id,
                        Mark::dirty(child_flag, Ext::Set(NodeExtFlags::NoFlags), false),
                    )
                    .await?;

                if is_directory {
                    mark_children_dirty_moved_recursive(
                        repository.clone(),
                        state.clone(),
                        child_id,
                        undelete,
                    )
                    .await?;
                }
            }

            Ok(())
        })
    }

    mark_children_dirty_moved_recursive(repository, state, parent_node, undelete).await
}

/// Recursively process a directory, marking each child as dirty based on filesystem state.
///
/// `nodes` names the directory in each tree, and every child is reached from it rather than
/// resolved from `dir_path`, which is the working-tree path the filter and the listing answer for.
///
/// `states` is the filter verdict for `dir_path`, which each child steps from.
///
/// The listing names everything the working tree holds here, so the pass that follows settles
/// which of the current revision's children are deletes from those names rather than by reading
/// the working tree a second time. The names are collected ahead of the filter, a child the
/// filter leaves out standing on disk all the same. A child the staged tree holds elsewhere is
/// no delete ([`held_elsewhere`]).
#[lore_macro::test_pub]
fn dirty_directory<'a>(
    walk: &'a DirtyWalk,
    nodes: DirtyNodes,
    dir_path: &'a RelativePath,
    states: FilterStates,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DirtyError>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = walk
            .operation
            .read_directory(dir_path)
            .await
            .forward_with::<DirtyError, _>(|| format!("Failed to read directory {dir_path}"))?;

        let deletes_to_find = nodes.current.is_valid_or_root_node_id();
        let mut present = Vec::new();
        let force = execution_context().globals().force();
        while let Some(entry) = entries.next().await {
            let entry = entry.forward::<DirtyError>("Failed to read directory entry")?;
            if is_reserved_node_name(&entry.name) {
                continue;
            }
            if deletes_to_find {
                present.push(entry.name_hash);
            }
            let child_path = dir_path.push_into_buf(&entry.name).freeze();

            // Looked up before the filter is asked: a node the tree tracks as the kind the entry is
            // is past the ignore slot. The trees are searched only where the filter would drop it.
            let child_nodes = nodes.child(walk, entry.name_hash).await;
            let is_directory = entry.info.is_dir();
            let tracked = !force
                && walk
                    .repository
                    .filter
                    .child_excludes_tree(states, &child_path, is_directory, FilterMode::Full)
                    .1
                && tracks(walk, child_nodes, Some(is_directory)).await?;
            let (child_states, excluded) = walk.repository.filter.child_emit_unless_forced(
                force,
                FilterMode::View,
                states,
                &child_path,
                is_directory,
                FilterMode::TRACKED.at(tracked),
            );
            if excluded {
                continue;
            }

            dirty_path(
                walk,
                child_nodes,
                staged_node(walk, child_nodes).await,
                StagedParent::node(nodes.staged),
                &child_path,
                entry.info,
                child_states,
            )
            .await?;
        }

        if deletes_to_find {
            present.sort_unstable();
            let children = walk
                .state_current
                .node_children(walk.repository.clone(), nodes.current)
                .await
                .forward::<DirtyError>("Failed to get directory children")?;

            for &child_id in &children {
                let Some((current, child_name)) = walk
                    .state_current
                    .node_and_name_or_skip(walk.repository.clone(), child_id)
                    .await
                    .forward::<DirtyError>("Failed to get child name")?
                else {
                    continue;
                };
                let name_hash = crate::hash::hash_string(&child_name);
                if present.binary_search(&name_hash).is_ok() {
                    continue;
                }
                let child_rel = dir_path.push_into_buf(&child_name).freeze();
                drop(child_name);

                let Some(child_states) = absent_child_states(walk, states, &child_rel, &current)
                else {
                    continue;
                };
                // The current side is the child being enumerated, so only the staged side
                // is looked up.
                let staged = subnode(
                    &walk.state_staged,
                    &walk.repository,
                    nodes.staged,
                    name_hash,
                )
                .await;
                if !staged.is_valid_or_root_node_id()
                    && held_elsewhere(walk, child_id, current.node_type()).await?
                {
                    continue;
                }
                let child_nodes = DirtyNodes {
                    current: child_id,
                    staged,
                };
                dirty_path(
                    walk,
                    child_nodes,
                    staged_node(walk, child_nodes).await,
                    StagedParent::node(nodes.staged),
                    &child_rel,
                    FileInfo::NotExist,
                    child_states,
                )
                .await?;
            }
        }

        Ok(())
    })
}

/// Whether the staged tree holds `node`, of `node_type`, a child of the current revision's
/// directory a walk reads that the working tree and the staged directory lack by its name: moved
/// or renamed elsewhere, not deleted.
async fn held_elsewhere(
    walk: &DirtyWalk,
    node: NodeID,
    node_type: LoreNodeType,
) -> Result<bool, DirtyError> {
    walk.state_staged
        .holds_node(&walk.repository, node, node_type)
        .await
        .forward::<DirtyError>("Failed to read a node the staged tree may hold elsewhere")
}

/// The view's verdict a child the working tree lacks carries into its marking, the current
/// revision holding it as `current`, or `None` where the view leaves it out, forced or not: the
/// working tree lacks what the view excludes without its having been deleted. The ignore file has
/// no say over a deletion.
fn absent_child_states(
    walk: &DirtyWalk,
    states: FilterStates,
    path: &RelativePath,
    current: &Node,
) -> Option<FilterStates> {
    let (child_states, out_of_view) = walk.repository.filter.child_excludes_tree(
        states,
        path,
        !current.is_file(),
        FilterMode::View,
    );
    (!out_of_view).then_some(child_states)
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
    // Lookups ignore case, so `A` to `a/b` moves a node below itself as well.
    if to_path.is_below_ignore_case(&from_path) {
        return Err(InvalidArguments {
            reason: "Cannot move a path into itself".into(),
        }
        .into());
    }

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    let from_node = node_outside_links(&state, &repository, from_path.as_str())
        .await
        .forward_with::<DirtyError, _>(|| format!("looking up the source {from_path}"))?;
    let to_parent_id = parent_outside_links(&state, &repository, &to_path)
        .await
        .forward_with::<DirtyError, _>(|| format!("looking up the parent of {to_path}"))?;

    let to_name = to_path.name();
    let name = if from_path.name() != to_name {
        NameChange::Rename(to_name)
    } else {
        NameChange::Keep
    };
    mark_moved(&repository, &state, from_node, to_parent_id, name).await?;

    save_staged(&repository, &state, current_revision).await
}

/// Marks each of `moves` dirty as moved again in `state_staged`, which holds each node where
/// `state_current`, the revision it is staged on, does; answers those it did not mark. `moves`
/// come in the order a walk of the tree they came from left them, each with its path in that tree,
/// so in their reverse each directory a path runs through is marked before the nodes below it.
/// Each node first takes a name of its own ([`State::node_park`]), so no path runs through a node
/// yet to be marked. A directory on a path the tree lacks is added, marked dirty as added.
///
/// A move that cannot be marked is skipped: one the tree no longer holds with its kind, such as
/// one a staged change displaced, and one whose path runs through or ends below something other
/// than a live directory, or ends inside the node's own subtree, which then takes its committed
/// name again. One warning names how many were skipped and one of them; the path replay marks
/// what the working tree holds at their paths. Every node is checked before any is marked, as a
/// directory added on a path may take a slot such a node left. Boxed by its caller, which takes it
/// only where there are moves to mark.
#[lore_macro::test_pub]
async fn mark_moved_again<'a>(
    repository: &Arc<RepositoryContext>,
    state_current: &State,
    state_staged: &Arc<State>,
    moves: &'a [MovedNode],
) -> Result<Vec<&'a MovedNode>, DirtyError> {
    let mut held = Vec::with_capacity(moves.len());
    for moved in moves {
        let holds = state_staged
            .holds_node(repository, moved.node, moved.node_type)
            .await
            .forward::<DirtyError>("Failed to read a node to mark moved")?;
        if holds {
            state_staged
                .node_park(repository, moved.node)
                .await
                .forward::<DirtyError>("Failed to rename a node to mark moved")?;
        }
        held.push(holds);
    }
    let mut skipped = Vec::new();
    for (moved, held) in moves.iter().zip(held).rev() {
        if held && mark_moved_to(repository, state_staged, moved).await? {
            continue;
        }
        if held {
            Box::pin(unpark(repository, state_current, state_staged, moved.node)).await?;
        }
        lore_debug!("Move to {} not marked again", moved.path);
        skipped.push(moved);
    }
    match &skipped[..] {
        [] => {}
        [moved] => lore_warn!(
            "The move to {}, marked dirty and not staged, is not kept past the commit; its path is \
             marked dirty as the working tree holds it",
            moved.path
        ),
        [.., moved] => lore_warn!(
            "{} moves marked dirty and not staged are not kept past the commit, the move to {} \
             among them; their paths are marked dirty as the working tree holds them",
            skipped.len(),
            moved.path
        ),
    }
    Ok(skipped)
}

/// Marks each node of `marks` in `state_staged` with the dirty marks it carries, where the tree
/// holds it and it is none of `skipped`, moves that could not be marked again; answers whether it
/// marked any. A move mark goes where a move marked again put one, as on a node below a directory
/// whose move the commit left out; elsewhere the revision took the move, so the node keeps its
/// other marks alone. Boxed by its caller, which takes it only where there are marks to carry.
async fn mark_carried(
    repository: &Arc<RepositoryContext>,
    state_staged: &State,
    marks: &[(NodeID, NodeFlags)],
    skipped: &[&MovedNode],
) -> Result<bool, DirtyError> {
    let move_bit = NodeFlags::DirtyMove & NodeFlags::ActionBits;
    let mut marked = false;
    for &(node, flags) in marks {
        if skipped.iter().any(|moved| moved.node == node)
            || !state_staged
                .holds_node(repository, node, flags.node_type())
                .await
                .forward::<DirtyError>("Failed to read a node to mark dirty again")?
        {
            continue;
        }
        let moved_again = state_staged
            .node(repository.clone(), node)
            .await
            .forward::<DirtyError>("Failed to read a node to mark dirty again")?
            .is_dirty_move();
        let mut carried = (flags & NodeFlags::DirtyBits) - move_bit;
        if moved_again {
            carried |= move_bit;
        }
        if !carried.intersects(NodeFlags::ActionBits) {
            continue;
        }
        let own = Mark::dirty(carried, Ext::Set(NodeExtFlags::NoFlags), false);
        state_staged
            .node_mark(repository, node, own)
            .await
            .forward::<DirtyError>("Failed to mark a node dirty again")?;
        marked = true;
    }
    Ok(marked)
}

/// Gives `node`, which [`mark_moved_again`] renamed and did not mark, the name `state_current`
/// holds it by again. Boxed by its caller, as only a skipped move takes it.
async fn unpark(
    repository: &Arc<RepositoryContext>,
    state_current: &State,
    state_staged: &State,
    node: NodeID,
) -> Result<(), DirtyError> {
    let name = state_current
        .node_name_clone(repository.clone(), node)
        .await
        .forward::<DirtyError>("Failed to read the committed name of a node")?;
    state_staged
        .node_rename(repository.clone(), node, NameChange::Rename(&name))
        .await
        .forward::<DirtyError>("Failed to rename a node back to its committed name")
}

/// Marks `moved`, which [`mark_moved_again`] renamed, dirty as moved again to its path, adding the
/// directories on it the tree lacks; answers `false`, having moved nothing, where the path runs
/// through or ends below something other than a live directory, or ends inside the node's own
/// subtree. A directory's path through the name it waits under runs into its own subtree, which is
/// answered before anything is added.
async fn mark_moved_to(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    moved: &MovedNode,
) -> Result<bool, DirtyError> {
    let is_directory = moved.node_type == LoreNodeType::Directory;
    let parent_path = moved.path.parent().unwrap_or_default();
    if is_directory
        && parent_path
            .split('/')
            .any(|name| crate::state::names_parked(moved.node, name))
    {
        return Ok(false);
    }
    let ParentChain::Reached(parent) =
        ensure_dirty_parent_dirs(repository, state_staged, ROOT_NODE, parent_path).await?
    else {
        return Ok(false);
    };
    if !state_staged
        .can_take(repository, parent, (moved.node, is_directory))
        .await
        .forward::<DirtyError>("Failed to read the directory to mark a node moved into")?
    {
        return Ok(false);
    }
    let name = NameChange::Rename(moved.path.name());
    mark_moved(repository, state_staged, moved.node, parent, name).await?;
    Ok(true)
}

/// Moves `node` to its new name, as `name` says, under `parent`, and marks the move dirty: the
/// node, as [`dirty_move_mark`] marks it, its subtree, and both directories with their ancestors.
async fn mark_moved(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    node: NodeID,
    parent: NodeID,
    name: NameChange<'_>,
) -> Result<(), DirtyError> {
    let (is_directory, deleted, dirty_move_flag) = {
        let node = state
            .node(repository.clone(), node)
            .await
            .forward::<DirtyError>("Failed deserializing state node block")?;
        (
            node.is_directory(),
            node.is_deleted(),
            dirty_move_mark(&node),
        )
    };
    let own = Mark::dirty(dirty_move_flag, Ext::Set(NodeExtFlags::NoFlags), true);
    let left = state
        .node_move(repository.clone(), node, parent, name, own)
        .await
        .forward::<DirtyError>("Failed to move the node")?;
    state
        .node_mark_destination(repository, parent, own)
        .await
        .forward::<DirtyError>("Failed to mark node as dirty move")?;

    if is_directory {
        mark_children_dirty_moved(repository.clone(), state.clone(), node, deleted)
            .await
            .forward::<DirtyError>("Failed to mark children as dirty moved")?;
    }

    state
        .node_mark_left(
            repository,
            (left, parent),
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await
        .forward::<DirtyError>("Failed to propagate dirty to source parent")
}

/// Saves `state` as the staged tree on top of `current_revision`, storing the staged anchor
/// where it differs from that revision; answers its signature.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn save_staged<'a>(
    repository: &'a Arc<RepositoryContext>,
    state: &'a Arc<State>,
    current_revision: Hash,
) -> impl Future<Output = Result<Hash, DirtyError>> + 'a {
    async move {
        let signature = serialize_staged(repository, state, current_revision)?
            .await
            .forward::<DirtyError>("Failed to serialize staged revision state")?;

        if signature != current_revision {
            crate::instance::store_staged_anchor(repository, signature)
                .await
                .forward::<DirtyError>("Failed to serialize staged anchor")?;
        }

        Ok(signature)
    }
}

/// Makes `state` the staged tree on top of `current_revision`, and answers the future that
/// serializes it, so a caller awaits the save alone.
fn serialize_staged<'a>(
    repository: &'a Arc<RepositoryContext>,
    state: &'a Arc<State>,
    current_revision: Hash,
) -> Result<impl Future<Output = Result<Hash, crate::state::StateError>> + Send + 'a, DirtyError> {
    state.reparent_onto(current_revision);
    let token = repository
        .try_write_token()
        .ok_or_else(|| DirtyError::internal("Marking dirty requires write access"))?;
    Ok(state.serialize(repository.clone(), token))
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

    node_outside_links(&state, &repository, from_path.as_str())
        .await
        .forward_with::<DirtyError, _>(|| format!("looking up the source {from_path}"))?;

    let to_name = to_path.name();
    let to_parent_id = parent_outside_links(&state, &repository, &to_path)
        .await
        .forward_with::<DirtyError, _>(|| format!("looking up the parent of {to_path}"))?;

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
        .node_mark(
            &repository,
            to_parent_id,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await
        .forward::<DirtyError>("Failed to propagate dirty to destination parent")?;

    save_staged(&repository, &state, current_revision).await
}
