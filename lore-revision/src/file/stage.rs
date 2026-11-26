// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::StreamExt;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::event;
use crate::file::existing_node_outside_links;
use crate::file::node_outside_links;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::layer;
use crate::link::LinkTracker;
use crate::link::LinkedState;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_limit_drain_tasks;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::status;
use crate::stage;
use crate::stage::KnownChild;
use crate::stage::LoreFileStageBeginEventData;
use crate::stage::LoreFileStageCountData;
use crate::stage::LoreFileStageEndEventData;
use crate::stage::LoreFileStageProgressEventData;
use crate::stage::LoreFileStageRevisionEventData;
use crate::stage::NameHashIndex;
use crate::stage::StageError;
use crate::stage::StageOptions;
use crate::stage::StageStats;
use crate::state;
use crate::state::Clear;
use crate::state::DirtyPaths;
use crate::state::Ext;
use crate::state::Mark;
use crate::state::NameChange;
use crate::state::NodeMapping;
use crate::state::State;
use crate::util::fan_out;
use crate::util::fan_out::AncestorNodes;
use crate::util::fan_out::longest_ancestor;
use crate::util::path::DepthPath;
use crate::util::path::RelativePath;
use crate::util::path::path_depth;

/// Where a walk has reached and where it is going: the point it starts from, what is left to
/// consume below it, and the case resolutions it may use on the way.
struct TreeWalkPath {
    /// The point the walk starts from.
    at: NodeMapping,
    /// What is left of the target below `at`, which the walk consumes.
    remainder: RelativePath,
    /// Resolutions memoized for the shared ancestors, keyed from the repository root, so they
    /// answer only for a walk that starts there.
    prefixes: Option<Arc<crate::util::fs::ResolvedPrefixes>>,
}

/// A shared ancestor once created: its node, the path it stands at once staged, which the walks
/// below it start from rather than the spelling they were given, and the linked repository and
/// State that hold it, where it lies inside a link.
#[lore_macro::test_pub]
struct CreatedAncestor {
    node: crate::node::NodeID,
    path: RelativePath,
    linked: Option<LinkedState>,
}

/// Where the walk for a target starts. Neither answer is a failure: a target with no created
/// ancestor is walked whole from the repository root.
#[lore_macro::test_pub]
enum WalkStart {
    /// The deepest ancestor that already has a node, at the path it was created at, and what is
    /// left of the target below it.
    BelowAncestor {
        path: RelativePath,
        node: crate::node::NodeID,
        linked: Option<LinkedState>,
        remainder: RelativePath,
    },
    /// The repository root, with the target untouched. The memo is keyed from that root, so this
    /// is the only walk it answers for.
    FromRoot(RelativePath),
}

/// Where the walk for `target` starts and what is left of it below that point.
///
/// The chain above the start is resolved once, while it is created, rather than once per target:
/// a metadata syscall and a node lookup per component per target, for an answer that does not
/// change.
///
/// Takes `target` by value so what is left below the start is a view of it rather than a second
/// path built from its bytes.
#[lore_macro::test_pub]
fn walk_start(
    mut target: RelativePath,
    ancestor_nodes: &AncestorNodes<'_, CreatedAncestor>,
) -> WalkStart {
    let Some((prefix, created)) = longest_ancestor(target.as_str(), ancestor_nodes) else {
        return WalkStart::FromRoot(target);
    };
    let (path, node, linked) = (created.path.clone(), created.node, created.linked.clone());
    target.pop_root_repeat(path_depth(prefix));
    WalkStart::BelowAncestor {
        path,
        node,
        linked,
        remainder: target,
    }
}

/// Spawn a stage task into the given layer's repository covering `remain` (the
/// path-suffix relative to the layer's mount). An empty `remain` stages the
/// layer's whole subtree.
#[allow(clippy::too_many_arguments)]
async fn stage_into_single_layer(
    operation: Arc<InstanceOperationImpl>,
    tasks: &mut JoinSet<Result<crate::node::NodeLink, StageError>>,
    layer: &crate::layer::Layer,
    layer_state: &crate::layer::LayerState,
    remain: RelativePath,
    stats: Arc<StageStats>,
    options: StageOptions,
) -> Result<(), StageError> {
    let layer_source_path = RelativePath::new_from_initial_path(&layer.source_path)
        .forward::<StageError>("Failed to construct layer source path")?;
    let layer_mount_path = RelativePath::new_from_initial_path(&layer.target_path)
        .forward::<StageError>("Failed to construct layer target path")?;

    // TODO(mjansson): If this has gone past a link into a subrepository, we
    // need to stage the link node and upwards in the layer repository.
    let layer_staged_node = layer_state
        .state_staged
        .locate_node(layer_state.repository.clone(), layer_source_path.as_str())
        .await
        .forward::<StageError>("Failed to locate layer source base node")?
        .held((
            layer_state.repository.clone(),
            layer_state.state_staged.clone(),
        ));

    lore_debug!(
        "Staging path in layer {}: {} / {}",
        layer.target_path,
        layer.source_path,
        remain
    );

    lore_spawn!(
        tasks,
        stage::stage_filesystem_path(
            operation,
            layer_staged_node.into_mapping(layer_mount_path),
            remain,
            KnownChild::Unresolved,
            stats,
            options,
            None, // No link tracking in layer staging
            None, // Layers don't have nested layer mounts (no overlap)
            None, // Prefixes are resolved against the repository root, not a layer
            None, // Node ids here index the layer's own state
        )
    );

    Ok(())
}

/// What every walk a stage spawns starts from: the trees it writes, the operation it
/// runs under, and the masking and case resolution its targets share.
struct StageWalk {
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    stats: Arc<StageStats>,
    link_tracker: Arc<LinkTracker>,
    global_mask: Option<Arc<Vec<String>>>,
    prefixes: Option<Arc<crate::util::fs::ResolvedPrefixes>>,
    options: StageOptions,
}

impl StageWalk {
    /// The trees this walk writes, mapped at `node` and the path it stands at.
    fn at(&self, path: RelativePath, node: crate::node::NodeID) -> NodeMapping {
        NodeMapping {
            repository: self.repository.clone(),
            state: self.state.clone(),
            path,
            node,
        }
    }

    /// Where a walk of `target` starts, and what it covers below that point, mapped into the
    /// trees this walk writes, or into the linked State holding the ancestor it starts at.
    fn walk(
        &self,
        target: RelativePath,
        ancestors: &AncestorNodes<'_, CreatedAncestor>,
    ) -> TreeWalkPath {
        match walk_start(target, ancestors) {
            WalkStart::BelowAncestor {
                path,
                node,
                linked,
                remainder,
            } => TreeWalkPath {
                at: match linked {
                    Some((repository, state)) => NodeMapping {
                        repository,
                        state,
                        path,
                        node,
                    },
                    None => self.at(path, node),
                },
                remainder,
                prefixes: None,
            },
            WalkStart::FromRoot(remainder) => TreeWalkPath {
                at: self.at(RelativePath::new(), ROOT_NODE),
                remainder,
                prefixes: self.prefixes.clone(),
            },
        }
    }

    /// The queue a walk starting at `at` leaves its discards in: the stage's own, which answers for
    /// the stage's own State, and none for a walk starting in a linked State, as for a walk that
    /// crosses a link.
    fn discards_at(
        &self,
        at: &NodeMapping,
        discards: &Arc<stage::DiscardQueue>,
    ) -> stage::PendingDiscards {
        (at.repository.id == self.repository.id).then(|| discards.clone())
    }
}

/// Resolve the case of every directory the targets share once, so no target under them
/// resolves it again.
///
/// Nothing is resolved under `Keep`, which stages by renaming the file system to match the
/// tree: the first such rename would leave the map naming a directory that is no longer
/// there.
async fn resolve_shared_prefixes(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    shared_ancestors: &[DepthPath],
    options: StageOptions,
) -> Result<Option<Arc<crate::util::fs::ResolvedPrefixes>>, StageError> {
    if matches!(options.case_change, stage::StageCaseChange::Keep) {
        return Ok(None);
    }

    let prefixes = Arc::new(
        crate::util::fs::resolve_prefixes(operation, repository.require_path()?, shared_ancestors)
            .await,
    );
    lore_debug!(
        "Resolved {} of {} shared ancestor prefixes",
        prefixes.len(),
        shared_ancestors.len()
    );
    Ok(Some(prefixes))
}

/// What the creation of a shared ancestor starts from.
struct AncestorWalk {
    /// The walk to the ancestor.
    walk: TreeWalkPath,
    /// The child of the walk's start that the first component of its remainder names.
    known: KnownChild,
    /// The path the ancestor resolved to before it was staged.
    resolved: RelativePath,
}

/// Create the node for every directory the targets share, by staging it without its
/// children, as [`fan_out::create_shared_ancestors`] orders it.
///
/// A directory a walk reaches across a link, the root of a repository mounted whole among them, is
/// recorded with the linked repository and State its stage reached, at the path the tree spells it
/// at, so the walks below it start there and do not cross the link again; one whose stage reaches
/// no node there, as a deletion, is left unrecorded, and the walks below it cross the link.
async fn precreate_shared_ancestors<'a>(
    walk: &StageWalk,
    shared_ancestors: &'a [DepthPath],
    discards: &Arc<stage::DiscardQueue>,
) -> Result<AncestorNodes<'a, CreatedAncestor>, StageError> {
    let mut options = walk.options;
    options.no_children = true;
    fan_out::create_shared_ancestors(
        shared_ancestors,
        |level, nodes| {
            resolve_level_children(
                level
                    .iter()
                    .map(|ancestor| {
                        let given = RelativePath::new_from_clean_parts(ancestor.path(), "");
                        AncestorWalk {
                            resolved: resolved_or_given(&given, walk.prefixes.as_deref()),
                            walk: walk.walk(given, nodes),
                            known: KnownChild::Unresolved,
                        }
                    })
                    .collect(),
            )
        },
        |_,
         _,
         AncestorWalk {
             walk: walk_path,
             known,
             resolved,
         }| {
            let start = walk_path.at.clone();
            let own = (walk.repository.clone(), walk.state.clone());
            let operation = walk.operation.clone();
            let stats = walk.stats.clone();
            let link_tracker = walk.link_tracker.clone();
            let global_mask = walk.global_mask.clone();
            let discards = walk.discards_at(&start, discards);
            async move {
                let mut reached = None;
                let link = stage::stage_filesystem_path_reaching(
                    operation,
                    walk_path.at,
                    walk_path.remainder,
                    known,
                    stats,
                    options,
                    Some(link_tracker),
                    global_mask,
                    walk_path.prefixes,
                    discards,
                    Some(&mut reached),
                )
                .await?;
                if let Some(reached) = reached {
                    return Ok(Some(CreatedAncestor {
                        node: reached.node,
                        path: reached.path,
                        linked: Some((reached.repository, reached.state)),
                    }));
                }
                if !link.is_valid() {
                    return Ok(None);
                }
                let path = created_path(&start, link.node).await?;
                Ok(Some(CreatedAncestor {
                    node: link.node,
                    path: path.unwrap_or(resolved),
                    linked: link
                        .crosses_link(&own.0, &own.1)
                        .then(|| (start.repository.clone(), start.state.clone())),
                }))
            }
        },
        |err| StageError::internal_with_context(err, "Failed to join pre-create task"),
    )
    .await
}

/// The child each walk of a level of shared ancestors names below its start, from one listing of
/// each node of one State two or more of the walks start at, as [`TargetDirectory`] lists a
/// directory several targets name a child of; a walk alone at its start is left to search. The
/// level is in path order, so the walks below one directory follow each other. Each creation of the
/// level adds only the child its own walk names, so the listing, taken before any of them starts,
/// answers for the rest. A node ID names a node of its own State only: the walks below two links
/// can start at one ID in two States.
async fn resolve_level_children(
    mut level: Vec<AncestorWalk>,
) -> Result<Vec<AncestorWalk>, StageError> {
    for run in level.chunk_by_mut(|left, right| {
        left.walk.at.node == right.walk.at.node
            && Arc::ptr_eq(&left.walk.at.state, &right.walk.at.state)
    }) {
        let [first, _, ..] = &*run else {
            continue;
        };
        let at = &first.walk.at;
        let directory = TargetDirectory::listed(
            stage::directory_children(&at.repository, &at.state, at.node).await,
        )?;
        for ancestor in run.iter_mut() {
            ancestor.known =
                directory.child(crate::hash::hash_string(ancestor.walk.remainder.root()));
        }
    }
    Ok(level)
}

/// `given` in the case `prefixes` resolved it to, where it resolved as a whole: a shorter match
/// answers for a shorter path and would drop components.
#[lore_macro::test_pub]
fn resolved_or_given(
    given: &RelativePath,
    prefixes: Option<&crate::util::fs::ResolvedPrefixes>,
) -> RelativePath {
    prefixes
        .and_then(|resolved| resolved.longest_prefix_of(given.as_str()))
        .filter(|(depth, _)| *depth == path_depth(given.as_str()))
        .map_or_else(
            || given.clone(),
            |(_, variation)| RelativePath::new_from_clean_parts(variation, ""),
        )
}

/// The path at which `created`, a node of `start`'s State, stands, spelled by the tree below
/// `start`, the point its walk started from.
///
/// Staging the ancestor made the tree and the file system agree on that spelling: `Keep` renames
/// the file system to the tree, `Rename` the tree to the file system. `None` where `start` does not
/// hold it, which leaves the caller the spelling it resolved before staging.
async fn created_path(
    start: &NodeMapping,
    created: crate::node::NodeID,
) -> Result<Option<RelativePath>, StageError> {
    let below = start
        .state
        .node_path_below(start.repository.clone(), created, start.node)
        .await
        .forward::<StageError>("Failed to resolve the path of a shared ancestor")?;
    Ok(below.map(|below| {
        if start.path.is_empty() {
            below
        } else {
            RelativePath::new_from_clean_parts(start.path.as_str(), below.as_str())
        }
    }))
}

/// What a stage's targets name a child of, in a directory enclosing the current target.
enum TargetDirectory {
    /// A directory no target names a child of yet.
    Unnamed,
    /// A directory one target names a child of.
    NamedOnce,
    /// The directory's children, listed once a second target names a child of it.
    Listed(NameHashIndex),
    /// A node of the walk's State that is not a directory, such as a link, which lists none.
    Unlisted,
}

impl TargetDirectory {
    /// The child of `directory` carrying `name_hash`, which a target names: from the directory's
    /// listing, listed once a second target names a child of it; unresolved for the first, and as
    /// [`Self::child`] answers.
    async fn known(
        &mut self,
        repository: &Arc<RepositoryContext>,
        state: &State,
        directory: NodeID,
        name_hash: u64,
    ) -> Result<KnownChild, StageError> {
        match self {
            Self::Unnamed => {
                *self = Self::NamedOnce;
                return Ok(KnownChild::Unresolved);
            }
            Self::NamedOnce => {
                *self =
                    Self::listed(stage::directory_children(repository, state, directory).await)?;
            }
            Self::Listed(_) | Self::Unlisted => {}
        }
        Ok(self.child(name_hash))
    }

    /// The directory a [`stage::directory_children`] answer lists: its children by name hash, or
    /// [`Self::Unlisted`] for a node that is not a directory.
    fn listed(
        listing: Result<Option<stage::DirectoryListing>, state::StateError>,
    ) -> Result<Self, StageError> {
        Ok(
            match listing.forward::<StageError>("Failed to list a directory the targets share")? {
                Some((listed, _)) => Self::Listed(NameHashIndex::new(listed)),
                None => Self::Unlisted,
            },
        )
    }

    /// The child carrying `name_hash` in the listing; unresolved where nothing is listed and
    /// where two children carry `name_hash`, so the stage walk's search answers, which takes a node
    /// not marked deleted before one that is ([`State::find_live_subnode`]).
    fn child(&self, name_hash: u64) -> KnownChild {
        let Self::Listed(children) = self else {
            return KnownChild::Unresolved;
        };
        let mut carrying = children.carrying(name_hash);
        let first = carrying.next();
        if carrying.next().is_none() {
            KnownChild::Resolved(first)
        } else {
            KnownChild::Unresolved
        }
    }
}

/// The directories enclosing a stage's current target that its targets name a child of, the
/// innermost last, with their paths, so the walk of a target below a directory several of them
/// name a child of takes the child its first component names from the directory's listing, as
/// given ([`KnownChild`]), rather than searching its chain, which N targets below one directory
/// would search N times. The targets come in subtree order, so those below a directory follow each
/// other, and a directory is dropped once a target leaves it. The targets are disjoint, and two
/// of them below one child of a directory start their walks below that child, which the stage
/// creates first, so no target's walk adds or moves the child another target names.
#[derive(Default)]
struct TargetDirectories(Vec<(RelativePath, NodeID, TargetDirectory)>);

impl TargetDirectories {
    /// The child of `at`, the node a target's walk starts at, that the first component of
    /// `remainder` names, as [`TargetDirectory::known`] answers it; unresolved for an empty
    /// remainder.
    async fn known(
        &mut self,
        at: &NodeMapping,
        remainder: &RelativePath,
    ) -> Result<KnownChild, StageError> {
        while self
            .0
            .last()
            .is_some_and(|(path, _, _)| !path.is_empty() && !path.covers_ignore_case(&at.path))
        {
            self.0.pop();
        }
        if remainder.is_empty() {
            return Ok(KnownChild::Unresolved);
        }
        if (self.0.last()).is_none_or(|(path, node, _)| *node != at.node || *path != at.path) {
            self.0
                .push((at.path.clone(), at.node, TargetDirectory::Unnamed));
        }
        let Some((_, _, named)) = self.0.last_mut() else {
            return Ok(KnownChild::Unresolved);
        };
        named
            .known(
                &at.repository,
                &at.state,
                at.node,
                crate::hash::hash_string(remainder.root()),
            )
            .await
    }
}

/// Spawn a walk per target into `tasks`.
///
/// The shared ancestors exist by now and the targets are disjoint, so every remaining
/// creation is single-writer or a distinct sibling, which `node_add` publishes with an
/// atomic CAS prepend. Reports the first failure, having stopped spawning at it; what is
/// already in flight is the caller's to drain.
async fn spawn_target_walks(
    walk: &StageWalk,
    antichain: Vec<RelativePath>,
    ancestors: &AncestorNodes<'_, CreatedAncestor>,
    discards: &Arc<stage::DiscardQueue>,
    tasks: &mut JoinSet<Result<crate::node::NodeLink, StageError>>,
) -> Option<StageError> {
    let mut failure = None;
    let mut directories = TargetDirectories::default();
    for target in antichain {
        let walk_path = walk.walk(target, ancestors);
        let known = match directories.known(&walk_path.at, &walk_path.remainder).await {
            Ok(known) => known,
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        let target_discards = walk.discards_at(&walk_path.at, discards);
        lore_spawn!(
            tasks,
            stage::stage_filesystem_path(
                walk.operation.clone(),
                walk_path.at,
                walk_path.remainder,
                known,
                walk.stats.clone(),
                walk.options,
                Some(walk.link_tracker.clone()),
                walk.global_mask.clone(),
                walk_path.prefixes,
                target_discards,
            )
        );
        if let Err(err) = lore_limit_drain_tasks!(
            tasks,
            MAX_CONCURRENT_TREE_TASKS,
            StageError::internal("Failed to join task")
        ) {
            failure = failure.or(Some(err));
        }
        if failure.is_some() {
            break;
        }
    }
    failure
}

/// The directories of `directories`, marked added or moved, that the working tree holds, which a
/// stage takes as themselves; each other is pushed onto `paths`, staged as any absent path is,
/// unless the paths below it carry it ([`carried_where_absent`]). Up to
/// [`MAX_CONCURRENT_TREE_TASKS`] directories are looked up at once, in order. Boxed by its caller,
/// which takes it only where the collection found such a directory, so its lookups stay out of the
/// stage's future.
async fn directories_held(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    directories: Vec<RelativePath>,
    paths: &mut Vec<RelativePath>,
) -> Result<Vec<RelativePath>, StageError> {
    let mut held = Vec::with_capacity(directories.len());
    let mut found = futures::stream::iter(directories)
        .map(|directory| async move {
            let holds = holds_directory(operation, &directory).await?;
            let staged = holds || !carried_where_absent(repository, state, &directory).await?;
            Ok::<_, StageError>((directory, holds, staged))
        })
        .buffered(MAX_CONCURRENT_TREE_TASKS);
    loop {
        let Some(directory) = found.next().await else {
            break;
        };
        match directory? {
            (directory, true, _) => held.push(directory),
            (directory, false, true) => paths.push(directory),
            (_, false, false) => {}
        }
    }
    Ok(held)
}

/// Whether the directory marked dirty at `path`, which the working tree no longer holds, is left to
/// the paths below it: a new one holding nodes, as they carry it
/// ([`state::marked_directory_carried_by_children`]). Any other is staged as an absent path is.
async fn carried_where_absent(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &RelativePath,
) -> Result<bool, StageError> {
    let Ok(held) = state
        .locate_held_node_below(repository.clone(), ROOT_NODE, path.as_str())
        .await
    else {
        return Ok(false);
    };
    let node = held
        .read()
        .await
        .forward::<StageError>("Failed to read a directory the working tree no longer holds")?;
    Ok(state::marked_directory_carried_by_children(&node))
}

/// Whether the working tree holds a directory at `path`, resolved as a stage resolves a target
/// ([`stage::stage_filesystem_path`]).
async fn holds_directory(
    operation: &InstanceOperationImpl,
    path: &RelativePath,
) -> Result<bool, StageError> {
    let (resolved, info) = crate::util::fs::filesystem_path_and_info(operation, "", path, None)
        .await
        .unwrap_or_else(|_| (path.clone(), None));
    let info = match info {
        Some(info) => info,
        None => operation
            .file_info(&resolved)
            .await
            .forward_any::<StageError>("Failed to describe a directory to stage")?,
    };
    Ok(info.is_dir())
}

/// The directories a stage creates before its walks, a depth at a time, by staging each without
/// its children ([`precreate_shared_ancestors`]): those two or more of `antichain` and `alone`
/// share ([`fan_out::shared_ancestors`]), and each of `alone`, the directories marked added or
/// moved the working tree holds, staged as themselves, their nodes and their parents alone. One an
/// entry of `antichain` covers is left to that entry's walk, and a shared directory one of `alone`
/// spells in another case gives way to it, so each node is created once.
#[lore_macro::test_pub]
fn created_first(antichain: &[RelativePath], alone: &[RelativePath]) -> Vec<DepthPath> {
    if alone.is_empty() {
        return fan_out::shared_ancestors(antichain);
    }
    let alone: Vec<&RelativePath> = (alone.iter())
        .filter(|directory| !directory.covered_in(antichain))
        .collect();
    let mut targets: Vec<&str> = (antichain.iter().map(RelativePath::as_str))
        .chain(alone.iter().map(|directory| directory.as_str()))
        .collect();
    targets.sort_unstable();
    let spelled: std::collections::HashSet<&str> = (alone.iter())
        .map(|directory| directory.as_lowercase_str())
        .collect();
    let mut created: Vec<DepthPath> = fan_out::shared_ancestors(&targets)
        .into_iter()
        .filter(|shared| !spelled.contains(shared.path().to_lowercase().as_str()))
        .collect();
    created.extend(
        alone
            .iter()
            .map(|directory| DepthPath::new(directory.as_str().to_string())),
    );
    created.sort_unstable();
    created
}

/// Spawn a walk per targeted layer into `tasks`, reporting the first failure as
/// [`spawn_target_walks`] does. Layer jobs run against their own separate states.
async fn spawn_layer_walks(
    walk: &StageWalk,
    layers: &[(crate::layer::Layer, crate::layer::LayerState)],
    layer_paths: Vec<(usize, RelativePath)>,
    tasks: &mut JoinSet<Result<crate::node::NodeLink, StageError>>,
) -> Option<StageError> {
    let mut failure = None;
    for (layer_index, remain) in layer_paths {
        let (layer, layer_state) = &layers[layer_index];
        if let Err(err) = stage_into_single_layer(
            walk.operation.clone(),
            tasks,
            layer,
            layer_state,
            remain,
            walk.stats.clone(),
            walk.options,
        )
        .await
        {
            failure = Some(err);
            break;
        }
        if let Err(err) = lore_limit_drain_tasks!(
            tasks,
            MAX_CONCURRENT_TREE_TASKS,
            StageError::internal("Failed to join task")
        ) {
            failure = failure.or(Some(err));
        }
        if failure.is_some() {
            break;
        }
    }
    failure
}

/// Drain every spawned walk, reporting progress while they run. The first failure wins,
/// `failure` included, so a spawn that stopped early is reported ahead of a join.
async fn drain_walks(
    tasks: &mut JoinSet<Result<crate::node::NodeLink, StageError>>,
    stats: &Arc<StageStats>,
    ticker: &mut tokio::time::Interval,
    mut failure: Option<StageError>,
) -> Option<StageError> {
    while !tasks.is_empty() {
        tokio::select! {
            _ = ticker.tick() => {
                event::LoreEvent::FileStageProgress(LoreFileStageProgressEventData {
                    count: LoreFileStageCountData::new(stats.clone()),
                }).send();
            },
            result = tasks.join_next() => {
                if let Some(result) = result {
                    failure = failure.or(result
                        .map_err(|e| StageError::internal_with_context(e, "Failed to join task"))
                        .flatten()
                        .err());
                }
            }
        }
    }
    failure
}

/// Normalize a path as given into one relative to the repository root.
///
/// A path that does not land inside the repository is reported ignored and
/// yields nothing, so a caller that skips it has already told the user why.
async fn normalize_stage_path(
    repository: &Arc<RepositoryContext>,
    path: &LoreString,
) -> Option<RelativePath> {
    let repository_path = repository.require_path().ok()?;
    let Ok(relative_path) = RelativePath::new_from_user_path(repository_path, path.as_str()) else {
        emit_path_ignore(path.as_str()).await;
        lore_debug!("Ignoring invalid path: {path}");
        return None;
    };
    Some(relative_path)
}

/// Marks in `state`, the tree a stage resolves its targets against, what the provider's diff
/// finds below each of `paths`, through a `status` scan that reports nothing
/// ([`status::scan_paths`]), and answers whether the scan held a node to discard or return, which
/// changes `state` with no target left to stage. A path that names nothing in the repository is
/// left to the resolution, which reports it, and a path inside a layer mount to what the layer's
/// tree holds marked.
#[lore_macro::test_pub]
async fn mark_through_provider_diff(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    state: &Arc<State>,
    layers: &[(layer::Layer, layer::LayerState)],
    layer_target_refs: &[&str],
    paths: &LoreArray<LoreString>,
) -> Result<bool, StageError> {
    let root = repository.require_path()?;
    let named: Vec<RelativePath> = paths
        .as_slice()
        .iter()
        .filter_map(|path| RelativePath::new_from_user_path(root, path.as_str()).ok())
        .filter(|path| {
            !matches!(
                classify_stage_path(path.as_str(), layer_target_refs),
                LayerRoute::Inside { .. }
            )
        })
        .collect();
    if named.is_empty() {
        return Ok(false);
    }
    let targets: Vec<Option<RelativePath>> = if named.iter().any(RelativePath::is_empty) {
        vec![None]
    } else {
        RelativePath::dedup_to_supersets(named)
            .into_iter()
            .map(Some)
            .collect()
    };
    status::scan_paths(
        operation.clone(),
        repository,
        &targets,
        state_current,
        state,
        &layer::mounts(layers).await,
        None,
    )
    .await
    .forward::<StageError>("marking what the provider's diff finds")
}

/// Stage `paths` into the staged revision and return its hash.
///
/// Each entry in `paths` is classified as either an individual file path or
/// a directory path (the repository root counts as a directory):
///
/// - **Individual file paths** are always reconciled against the filesystem.
///   The file is read and its current state is staged regardless of dirty
///   flags. [`StageOptions::scan`] has no effect on these paths.
/// - **Directory paths** by default stage only the files and child
///   directories currently marked dirty in the repository state — this is
///   the fast path and relies on prior notifications or `status --scan`
///   calls to keep dirty flags accurate. Over a provider that holds its state
///   in a `State`, whose diff is exact, the stage first marks them through
///   that diff, so nothing prior is needed. When [`StageOptions::scan`] is
///   `true`, or the stage is forced, the directory is walked recursively on the
///   filesystem, every contained file is reconciled, and the dirty flags are
///   disregarded.
pub async fn stage(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    mut options: StageOptions,
) -> Result<Hash, StageError> {
    options.scan |= execution_context().globals().force();
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<StageError>("Failed to deserialize revision state")?;
    // Save the current revision before any modifications — the staged state
    // may share the same Arc<State> and modifications would change both.
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    let layers = {
        let mut layers = vec![];
        let list = layer::list(repository.clone()).await.unwrap_or_default();
        for layer in list {
            let layer_state = layer
                .deserialize_current_and_staged(repository.clone())
                .await
                .forward::<StageError>("Failed to deserialize layer state")?;

            layers.push((layer, layer_state));
        }
        layers
    };

    event::LoreEvent::FileStageBegin(LoreFileStageBeginEventData {
        path_count: paths.len(),
    })
    .send();

    lore_debug!("Stage options: {:?}", options);

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    let stats = Arc::new(StageStats::default());
    let link_tracker = LinkTracker::new();
    // Where nothing is staged yet `state` is `state_current` itself, which still answers
    // exactly: a walk adds nodes only for entries the file system holds.
    let discards = Arc::new(stage::DiscardQueue::new(state_current.clone()));

    // Every layer mount is staged by its own task, never the parent walk, so
    // masking every layer subtree on every main-repo walk is correct: an entry
    // not under a given target is never reached anyway.
    let global_mask: Option<Arc<Vec<String>>> = (!layers.is_empty())
        .then(|| Arc::new(layer::mount_paths(layers.iter().map(|(layer, _)| layer))));
    let layer_target_refs: Vec<&str> = global_mask
        .as_deref()
        .map_or_default(|paths| paths.iter().map(String::as_str).collect());

    // One filesystem operation covers the whole stage: a layer or link at a subpath
    // is a subtree of the same filesystem and takes the operation its parent holds.
    let walked = with_operation(repository.file_system(), async |operation| {
        let diff_discarded = operation.holds_state()
            && !options.scan
            && mark_through_provider_diff(
                &operation,
                &repository,
                &state_current,
                &state,
                &layers,
                &layer_target_refs,
                &paths,
            )
            .await?;

        let RoutedTargets {
            mut current_repository_paths,
            current_repository_directories,
            layer_paths,
        } = route_and_resolve_targets(
            &repository,
            &state_current,
            &state,
            &paths,
            &layer_target_refs,
            options,
        )
        .await?;
        let alone = if current_repository_directories.is_empty() {
            Vec::new()
        } else {
            Box::pin(directories_held(
                &operation,
                &repository,
                &state,
                current_repository_directories,
                &mut current_repository_paths,
            ))
            .await?
        };

        // A root target covers the whole tree; otherwise collapse overlaps so a
        // parent target subsumes anything that would be staged beneath it.
        let stage_root = current_repository_paths.iter().any(|p| p.is_empty());
        let antichain: Vec<RelativePath> = if stage_root {
            vec![RelativePath::new()]
        } else {
            RelativePath::dedup_to_supersets(current_repository_paths)
        };
        let shared_ancestors = created_first(&antichain, &alone);
        let main_count = antichain.len() + shared_ancestors.len();
        // A layer may be targeted by several paths; serialize each only once.
        let staged_layers: std::collections::BTreeSet<usize> = layer_paths
            .iter()
            .map(|(layer_index, _)| *layer_index)
            .collect();

        let walk = StageWalk {
            prefixes: resolve_shared_prefixes(&operation, &repository, &shared_ancestors, options)
                .await?,
            operation,
            repository: repository.clone(),
            state: state.clone(),
            stats: stats.clone(),
            link_tracker: link_tracker.clone(),
            global_mask: global_mask.clone(),
            options,
        };

        let ancestors = precreate_shared_ancestors(&walk, &shared_ancestors, &discards).await?;

        let mut tasks: JoinSet<Result<crate::node::NodeLink, StageError>> = JoinSet::new();
        let mut failure =
            spawn_target_walks(&walk, antichain, &ancestors, &discards, &mut tasks).await;
        if failure.is_none() {
            failure = spawn_layer_walks(&walk, &layers, layer_paths, &mut tasks).await;
        }
        if let Some(err) = drain_walks(&mut tasks, &stats, &mut ticker, failure).await {
            return Err(err);
        }
        if !Arc::ptr_eq(&state, &state_current) {
            state.hold_base(state_current.clone());
        }
        if state.holds_promoted_moves() {
            let promoted = Box::pin(stage_promoted_moves(
                &repository,
                state_current.revision(),
                &state,
            ))
            .await?;
            stats
                .directory_move_count
                .fetch_add(promoted, Ordering::Relaxed);
        }
        Ok::<_, StageError>((diff_discarded, main_count, staged_layers))
    })
    .await?;
    let (diff_discarded, main_count, staged_layers) = walked;

    let queued = discards.take();
    let discarded = !queued.is_empty() || diff_discarded;
    state::apply_pending_discards(state.clone(), repository.clone(), queued)
        .await
        .forward::<StageError>("Failed to discard entries no commit holds")?;

    let layer_staged: Vec<_> = staged_layers
        .into_iter()
        .map(|layer_index| (&layers[layer_index].0, &layers[layer_index].1))
        .collect();

    let count = LoreFileStageCountData::new(stats.clone());
    let total_count = count.total_count;
    event::LoreEvent::FileStageEnd(LoreFileStageEndEventData { count }).send();

    // A discard stages nothing and so raises no count, but it does mutate the
    // tree, and the mutation is only kept if the state is serialized below.
    if total_count == 0 && !discarded {
        return Ok(state.revision());
    }

    let mut staged_revision = state.revision();
    // Only update parent staged metadata if the walker actually mutated the
    // parent's state. With the layer-routing dispatch a parent task may be
    // spawned for an `AncestorOf` path even when every child is a layer mount
    // (mask-skipped) and no parent files changed; in that case we must NOT
    // bump the staged anchor because the resulting hash would diverge from
    // current_revision purely from set_revision_number/set_parent_self
    // metadata writes, tricking commit into trying to commit an empty parent.
    let parent_mutated = (main_count > 0 || diff_discarded)
        && (state.is_dirty() || link_tracker.has_modifications());
    if discarded && leaves_nothing_staged(&repository, &state, &link_tracker).await? {
        // A discard can leave nothing staged or dirty, which an unstage leaves with no staged
        // state at all.
        if !execution_context().globals().dry_run() {
            crate::instance::delete_staged_anchor(&repository)
                .await
                .forward::<StageError>("Failed to remove staged anchor")?;
        }
        staged_revision = current_revision;
    } else if parent_mutated {
        // Process links that need reserialization due to downstream changes
        stage::process_link_updates(
            repository.clone(),
            token,
            state.clone(),
            link_tracker.clone(),
        )
        .await?;

        // Before the save, with every walk drained, as nothing else frees a slot a walk left
        // detached or in flight (C15).
        state
            .node_sweep_detached(&repository)
            .await
            .forward::<StageError>("Failed to free the detached slots")?;

        state.reparent_onto(current_revision);

        let signature = state
            .serialize(repository.clone(), token)
            .await
            .forward::<StageError>("Failed to serialize staged revision state")?;

        if signature != current_revision {
            staged_revision = signature;
            if !execution_context().globals().dry_run() {
                crate::instance::store_staged_anchor(&repository, signature)
                    .await
                    .forward::<StageError>("Failed to serialize staged anchor")?;
            }
        }

        event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
            repository: repository.id,
            revision: signature,
        })
        .send();
    }

    for (layer, layer_state) in layer_staged {
        let state = layer_state.state_staged.clone();

        // A staged state never hashes equal to the committed current, so
        // pinning an unmutated layer pins a staged revision with nothing in it.
        if !state.is_dirty() {
            lore_debug!(
                "Layer at {} has no staged modifications, leaving staged state {} untouched",
                layer.target_path,
                layer.staged
            );
            continue;
        }

        state.reparent_onto(layer_state.state_current.revision());

        let signature = state
            .serialize(layer_state.repository.clone(), token)
            .await
            .forward::<StageError>("Failed to serialize staged revision state")?;

        if signature != layer.current && !execution_context().globals().dry_run() {
            layer::store_layer_staged(
                repository.clone(),
                token,
                layer.target_path.as_str(),
                layer.repository,
                signature,
            )
            .await
            .forward::<StageError>("Failed to serialize new layer state")?;
        }

        lore_debug!(
            "Stored staged state {} for layer at {} currently at {}",
            signature,
            layer.target_path,
            layer.current
        );

        event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
            repository: layer_state.repository.id,
            revision: signature,
        })
        .send();
    }

    Ok(staged_revision)
}

/// Whether `state` is left with nothing to keep a staged state for: no staged or dirty node, no
/// link change still to be staged on its link node, and no merge, cherry-pick or revert, which is
/// staged whatever its nodes hold.
async fn leaves_nothing_staged(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    link_tracker: &LinkTracker,
) -> Result<bool, StageError> {
    Ok(!link_tracker.has_modifications()
        && !state.is_merge_or_cherry_pick_or_revert()
        && !state
            .node_has_staged_or_dirty_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<StageError>("Failed to read the staged root")?)
}

/// What a stage target list resolved to.
struct RoutedTargets {
    /// Paths to walk in the current repository.
    current_repository_paths: Vec<RelativePath>,
    /// Directories of the current repository staged as themselves ([`DirtyPaths::directories`]).
    current_repository_directories: Vec<RelativePath>,
    /// Layer index, and the path relative to that layer's mount. An empty path
    /// stages the layer's whole subtree.
    ///
    /// Ordered by input, so the failure reported when several layers fail is the
    /// one belonging to the earliest target given.
    layer_paths: Vec<(usize, RelativePath)>,
}

/// Route each of `paths` through the configured layers and resolve what it
/// stages in `state`, the staged tree on `state_current`.
///
/// A path inside a layer belongs to that layer alone and resolves to nothing
/// here. A path a layer sits under takes the layers below it and resolves as
/// well. A path disjoint from every layer only resolves.
///
/// Resolving one is a tree lookup per component with no shared state, so the
/// whole list resolves at once, bounded by [`MAX_CONCURRENT_TREE_TASKS`]. Routing stays
/// in the loop instead: it is string work against the layer mounts, and it
/// appends to lists a task would need a lock to reach. The resolved paths need
/// no order, since the caller collapses them into a sorted antichain, and their
/// list is sized by the input count, most targets resolving to themselves.
///
/// A failure is the first to land rather than the first in input order, and does
/// not return until every resolution in flight has finished, since each is
/// reading state it has to finish reading.
async fn route_and_resolve_targets(
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    state: &Arc<State>,
    paths: &LoreArray<LoreString>,
    layer_target_refs: &[&str],
    options: StageOptions,
) -> Result<RoutedTargets, StageError> {
    let mut routed = RoutedTargets {
        current_repository_paths: Vec::with_capacity(paths.len()),
        current_repository_directories: Vec::new(),
        layer_paths: Vec::new(),
    };
    let mut left = Vec::new();
    let moves_possible = !Arc::ptr_eq(state, state_current);
    let mut resolve_tasks = JoinSet::new();
    let mut failure: Option<StageError> = None;
    let join_failure = |err: tokio::task::JoinError| {
        StageError::internal_with_context(err, "Failed to join target resolution task")
    };

    for path in paths.as_slice().iter() {
        if failure.is_some() {
            break;
        }
        let Some(relative_path) = normalize_stage_path(repository, path).await else {
            continue;
        };

        match classify_stage_path(relative_path.as_str(), layer_target_refs) {
            LayerRoute::Inside {
                layer_index,
                remain,
            } => {
                routed.layer_paths.push((layer_index, remain));
                continue;
            }
            LayerRoute::AncestorOf { layer_indices } => {
                for layer_index in layer_indices {
                    routed.layer_paths.push((layer_index, RelativePath::new()));
                }
            }
            LayerRoute::Disjoint => {}
        }

        lore_spawn!(
            resolve_tasks,
            resolve_stage_target(
                repository.clone(),
                state.clone(),
                relative_path,
                options,
                moves_possible
            )
        );
        fan_out::join_below(
            &mut resolve_tasks,
            MAX_CONCURRENT_TREE_TASKS,
            &mut failure,
            join_failure,
            |resolved| {
                resolved.map(|(target, path)| {
                    target.collect_into(&mut routed);
                    left.extend(path);
                })
            },
        )
        .await;
    }

    fan_out::join_below(
        &mut resolve_tasks,
        1,
        &mut failure,
        join_failure,
        |resolved| {
            resolved.map(|(target, path)| {
                target.collect_into(&mut routed);
                left.extend(path);
            })
        },
    )
    .await;
    if let Some(err) = failure {
        return Err(err);
    }
    if moves_possible && !left.is_empty() {
        Box::pin(resolve_moved_destinations(
            repository,
            state_current,
            state,
            left,
            options,
            &mut routed,
        ))
        .await?;
    }
    Ok(routed)
}

/// Adds to `routed` what a stage of the new path of each node a move took from one of `left`
/// resolves to ([`moved_destination`]), `left` being the paths of the targets that `state`, staged
/// on `state_current`, holds no node at or an added or a moved one. Boxed by its caller, which takes
/// it only for a stage naming such a path.
async fn resolve_moved_destinations(
    repository: &Arc<RepositoryContext>,
    state_current: &State,
    state: &State,
    left: Vec<RelativePath>,
    options: StageOptions,
    routed: &mut RoutedTargets,
) -> Result<(), StageError> {
    for path in &left {
        if let Some(destination) =
            moved_destination(repository, state_current, state, path, options).await?
        {
            destination.collect_into(routed);
        }
    }
    Ok(())
}

/// What one stage target resolves to.
///
/// A target that is not a directory being descended - a single file, a `scan`
/// target, a path with no node - resolves to itself, and a targets file is
/// mostly those. Naming that case keeps the common result off the heap.
enum ResolvedTarget {
    Single(RelativePath),
    Multiple(DirtyPaths),
}

impl ResolvedTarget {
    fn collect_into(self, routed: &mut RoutedTargets) {
        match self {
            ResolvedTarget::Single(path) => routed.current_repository_paths.push(path),
            ResolvedTarget::Multiple(collected) => {
                routed.current_repository_paths.extend(collected.paths);
                routed
                    .current_repository_directories
                    .extend(collected.directories);
            }
        }
    }
}

/// Resolve `relative_path` to the concrete set of repository-relative paths to
/// stage. Without `scan`, a directory resolves to the paths
/// [`State::collect_dirty_paths`] collects at it (none when nothing is dirty);
/// `scan`, single files, paths with no node, and a directory marked
/// dirty as deleted resolve to the path itself, the last so that its deletion is
/// staged whole, as [`State::collect_dirty_paths`] records one below.
///
/// Answers the path as well where it may be one a move left, which the staged tree holds no node
/// at, or an added or a moved one, for the caller to resolve the move's new path
/// ([`moved_destination`]). Such a path naming a directory to collect is a cold path, collected
/// boxed, so the path the answer keeps is not held beside the collection of every directory.
///
/// `moves_possible` is false where nothing is staged or marked, so that no dirty move can have left
/// a path: a `scan` target then resolves to itself with no lookup.
///
/// The lookup follows link mounts, and a node it reaches through a link is read
/// in the state holding it, otherwise a colliding block at the same coordinates
/// in the parent state would misclassify the target. The returned paths stay
/// parent-relative, since the filesystem walk traverses links itself.
async fn resolve_stage_target(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    options: StageOptions,
    moves_possible: bool,
) -> Result<(ResolvedTarget, Option<RelativePath>), StageError> {
    if options.scan && !moves_possible {
        return Ok((ResolvedTarget::Single(relative_path), None));
    }
    if relative_path.is_empty() {
        let target = resolve_held_target(
            (&state, &repository, ROOT_NODE),
            true,
            relative_path,
            options,
        )
        .await?;
        return Ok((target, None));
    }
    let Ok(held) = state
        .locate_held_node_below(repository.clone(), ROOT_NODE, relative_path.as_str())
        .await
    else {
        let left = Some(relative_path.clone());
        return Ok((ResolvedTarget::Single(relative_path), left));
    };
    if !held.id().is_valid_node_id() {
        return Ok((ResolvedTarget::Single(relative_path), None));
    }
    let (collects, left) = {
        let node = held
            .read()
            .await
            .forward::<StageError>("Failed to resolve node for dirty staging")?;
        (
            node.is_directory() && !node.is_dirty_delete(),
            node.is_added() || node.is_dirty_move() || node.is_staged_move(),
        )
    };
    let (id, (resolved_repository, resolved_state)) = held.into_parts();
    let held = (&*resolved_state, &resolved_repository, id);
    if !left {
        let target = resolve_held_target(held, collects, relative_path, options).await?;
        return Ok((target, None));
    }
    let target = if collects && !options.scan {
        Box::pin(resolve_held_target(
            held,
            collects,
            relative_path.clone(),
            options,
        ))
        .await?
    } else {
        ResolvedTarget::Single(relative_path.clone())
    };
    Ok((target, Some(relative_path)))
}

/// What a stage of `path`, where `held` holds a node, resolves to: without `scan`, the paths
/// [`State::collect_dirty_paths`] collects at it where it `collects`, a directory not marked
/// deleted; the path itself otherwise.
async fn resolve_held_target(
    held: (&State, &Arc<RepositoryContext>, NodeID),
    collects: bool,
    path: RelativePath,
    options: StageOptions,
) -> Result<ResolvedTarget, StageError> {
    if options.scan || !collects {
        return Ok(ResolvedTarget::Single(path));
    }
    let (state, repository, node) = held;
    Ok(ResolvedTarget::Multiple(
        state
            .collect_dirty_paths(repository.clone(), node, path)
            .await
            .forward::<StageError>("Failed to collect dirty paths")?,
    ))
}

/// What a stage of the new path of the node a move took from `path` resolves to, where
/// `state_current`, the revision `state` is staged on, holds a node there that `state` holds
/// moved elsewhere ([`stage::moved_away_from`]): the new path itself, and, without `scan`, the
/// dirty paths collected below a directory there, as [`resolve_stage_target`] resolves a path a
/// stage names. `None` for any other path.
async fn moved_destination(
    repository: &Arc<RepositoryContext>,
    state_current: &State,
    state: &State,
    path: &RelativePath,
    options: StageOptions,
) -> Result<Option<ResolvedTarget>, StageError> {
    let Some(moved) = stage::moved_away_from(repository, state_current, state, path).await? else {
        return Ok(None);
    };
    let destination = state
        .node_path(repository.clone(), moved)
        .await
        .forward::<StageError>("Failed to resolve the path a dirty move took a node to")?;
    let destination = RelativePath::new_from_clean_parts(&destination, "");
    let collects = state
        .node(repository.clone(), moved)
        .await
        .forward::<StageError>("Failed to read the node a dirty move took")?
        .is_directory();
    Ok(Some(
        resolve_held_target((state, repository, moved), collects, destination, options).await?,
    ))
}

/// Stages whole the move of each directory marked dirty as moved whose move a staged climb in
/// `state` staged with a node staged or arriving below it ([`State::take_promoted_moves`]), against
/// `base`, the revision `state` is staged on ([`stage_directory_move`]), whichever route staged the
/// node: a stage inside the directory, a staged move into it, or a dirty move of a staged node into
/// it. A directory gone from the tree since, or no longer a staged move, is passed over. Answers
/// how many moves it staged.
pub async fn stage_promoted_moves(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    state: &State,
) -> Result<u64, StageError> {
    let mut name = String::new();
    let mut staged = 0;
    loop {
        let promoted = state.take_promoted_moves();
        if promoted.is_empty() {
            return Ok(staged);
        }
        for directory in promoted {
            let node = state
                .node(repository.clone(), directory)
                .await
                .forward::<StageError>("Failed to read a directory whose move a climb staged")?;
            if node.is_discarded() || !node.is_directory() || !node.is_staged_move() {
                continue;
            }
            Box::pin(stage_directory_move(
                repository, base, state, directory, &mut name,
            ))
            .await?;
            staged += 1;
        }
    }
}

/// [`stage_promoted_moves`] of `state`, against the revision it was loaded from, under the State's
/// moves permit, as staging a directory's move marks its children: `permit`, where the caller holds
/// it, and one it takes otherwise. The queue is the State's, so a call stages a move another
/// call's climb queued as well, which that call then finds staged. Boxed by its callers, which
/// take it only where a climb promoted a move ([`State::holds_promoted_moves`]).
pub async fn stage_promoted_moves_of_call(
    repository: &Arc<RepositoryContext>,
    state: &State,
    permit: Option<&crate::state::MovesPermit<'_>>,
) -> Result<u64, StageError> {
    let _taken = match permit {
        Some(_) => None,
        None => Some(
            state
                .moves_permit()
                .await
                .forward::<StageError>("Failed to take the moves permit")?,
        ),
    };
    stage_promoted_moves(repository, state.built_on(), state).await
}

/// Stages the move of `directory`, marked dirty as moved, whole: it and each node below it that
/// moves with it alone, not staged and standing beside its parent where `base` holds it, or where
/// `base` holds no node of its kind there, take a staged move in place of their dirty one, as
/// [`stage_move`] marks them. A change of a node's own stays dirty: a move of its own, an add, a
/// deletion, and an edit, which a node that moves with the directory keeps as a modification
/// alone, with no staged mark beside it, as the two halves share their action bits. `name` is a
/// buffer kept across nodes.
pub(crate) async fn stage_directory_move(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    state: &State,
    directory: NodeID,
    name: &mut String,
) -> Result<(), StageError> {
    let moved_alone = NodeFlags::DirtyMove.bits();
    let move_bit = NodeFlags::DirtyMove & NodeFlags::ActionBits;
    state
        .node_mark(
            repository,
            directory,
            Mark::staged(NodeFlags::StagedMove, true),
        )
        .await
        .forward::<StageError>("Failed to stage the move of a directory")?;
    let mut staged = vec![directory];
    let mut next = 0;
    while let Some(&parent) = staged.get(next) {
        next += 1;
        let children = state
            .node_children(repository.clone(), parent)
            .await
            .forward::<StageError>("Failed to read a moved directory's children")?;
        for child in children {
            let node = state
                .node(repository.clone(), child)
                .await
                .forward::<StageError>("Failed to read a node of a moved directory")?;
            if !node.is_dirty_move()
                || node.is_staged()
                || !state
                    .at_base_place(repository, base, (child, &node), name)
                    .await
                    .forward::<StageError>("Failed to read the base revision")?
                    .unwrap_or(true)
            {
                continue;
            }
            let dirty = NodeFlags::from_bits_retain(node.flags) & NodeFlags::DirtyBits;
            if dirty.bits() == moved_alone {
                state
                    .node_mark(repository, child, Mark::staged(NodeFlags::StagedMove, true))
                    .await
                    .forward::<StageError>("Failed to stage a node moved with its directory")?;
                if node.is_directory() {
                    staged.push(child);
                } else {
                    state
                        .node_clear(repository, child, Clear::Dirty)
                        .await
                        .forward::<StageError>("Failed to clear a move its directory stages")?;
                }
            } else {
                state
                    .node_mark(
                        repository,
                        child,
                        Mark::dirty(dirty - move_bit, Ext::Keep, true),
                    )
                    .await
                    .forward::<StageError>("Failed to keep the change of a moved node")?;
            }
        }
    }
    for directory in staged.into_iter().rev() {
        state
            .node_clear(repository, directory, Clear::Vacated)
            .await
            .forward::<StageError>("Failed to clear a move its directory stages")?;
    }
    Ok(())
}

/// Marks every node below the moved directory `parent_node` staged as moved, with `move_flag`. A
/// node added, staged or dirty, stays an addition. A node deleted, staged or dirty, is left as it
/// is and not descended into, as a mark of the move would take the deletion off it: the two halves
/// share their action bits. Where the moved directory was deleted itself (`undelete`), every node
/// below carries its deletion, which the move takes off them all. One walk of the subtree
/// ([`State::node_mark_subtree`]), boxed by this function, as only a directory's move takes it.
async fn mark_children_moved(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    parent_node: crate::node::NodeID,
    move_flag: NodeFlags,
    undelete: bool,
) -> Result<(), crate::state::StateError> {
    Box::pin(
        state.node_mark_subtree(repository, parent_node, |_, child| {
            let child_flag = if child.is_added() {
                NodeFlags::StagedAdd
            } else {
                move_flag
            };
            (undelete || !child.is_deleted()).then_some(Mark::staged(child_flag, false))
        }),
    )
    .await
}

/// Marks each of `paths` as merged, reporting the counts staged so far while one runs.
///
/// One ticker covers the whole run rather than one per path, so the progress reported is
/// periodic in time rather than in paths.
async fn stage_merge_paths(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    state_stage: &Arc<State>,
    state_merge: &Arc<State>,
    paths: &LoreArray<LoreString>,
    stats: &Arc<StageStats>,
) -> Result<(), StageError> {
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    for path in paths.as_slice() {
        let Some(relative_path) = normalize_stage_path(repository, path).await else {
            continue;
        };

        // TODO(mjansson): Layers

        // TODO(vri): UCS-17955 - Merging and conflict resolution for links
        let mut task = lore_spawn!(stage::stage_merge_path(
            operation.clone(),
            repository.clone(),
            state_stage.clone(),
            state_merge.clone(),
            relative_path.clone(),
            stats.clone(),
        ));

        let result = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    event::LoreEvent::FileStageProgress(LoreFileStageProgressEventData {
                        count: LoreFileStageCountData::new(stats.clone()),
                    }).send();
                },
                result = &mut task => {
                    break result.map_err(|e| StageError::internal_with_context(e, "Failed to join task"))?;
                }
            }
        };

        result?;
    }
    Ok(())
}

pub async fn stage_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<Hash, StageError> {
    let (state_current, state_staged, _branch) =
        state::State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<StageError>("Failed to deserialize revision state")?;
    let state_stage = state_staged.unwrap_or(state_current);

    if !state_stage.is_merge() || state_stage.revision_number() != 0 {
        return Err(StageError::internal("Not in a pending merge"));
    }

    let state_merge = State::deserialize(repository.clone(), state_stage.parent_other())
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    event::LoreEvent::FileStageBegin(LoreFileStageBeginEventData {
        path_count: paths.len(),
    })
    .send();

    let stats = Arc::new(StageStats::default());
    // One operation covers every path: one per path would freeze a filesystem per path.
    // Nothing is written: the conflict is recorded in the staged state.
    with_operation(repository.file_system(), async |operation| {
        stage_merge_paths(
            &operation,
            &repository,
            &state_stage,
            &state_merge,
            &paths,
            &stats,
        )
        .await
    })
    .await?;

    // TODO(vri): UCS-17955 - Merging and conflict resolution for links
    // Serialize all staged links states recursively

    let signature = state_stage
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged revision state")?;
    if !execution_context().globals().dry_run() {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<StageError>("Failed to serialize staged anchor")?;
    }

    event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
        repository: repository.id,
        revision: signature,
    })
    .send();

    Ok(signature)
}

/// Stages the move of `from_path` to `to_path`, which the working tree already holds moved.
///
/// One operation covers the move: measuring the target and staging the directory it lands in both
/// read the working tree, and a filesystem holds one operation at a time.
#[allow(clippy::too_many_arguments)]
pub async fn stage_move(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    from_path: String,
    to_path: String,
    options: StageOptions,
) -> Result<Hash, StageError> {
    event::LoreEvent::FileStageBegin(LoreFileStageBeginEventData { path_count: 1 }).send();

    let from_path =
        RelativePath::new_from_user_path(repository.require_path()?, from_path.as_str())
            .forward_with::<StageError, _>(|| format!("Invalid path {from_path}"))?;
    let to_path = RelativePath::new_from_user_path(repository.require_path()?, to_path.as_str())
        .forward_with::<StageError, _>(|| format!("Invalid path {to_path}"))?;
    lore_debug!(
        "Stage move {} -> {} in repository {}",
        from_path.as_str(),
        to_path.as_str(),
        repository.path_for_display()
    );

    if from_path.as_str() == to_path.as_str() {
        return Err(StageError::internal("Cannot move a path to itself"));
    }
    // Lookups ignore case, so `A` to `a/b` moves a node below itself as well.
    if to_path.is_below_ignore_case(&from_path) {
        return Err(crate::errors::InvalidArguments {
            reason: "Cannot move a path into itself".into(),
        }
        .into());
    }
    if to_path.is_empty() {
        return Err(crate::errors::InvalidArguments {
            reason: "Cannot move a path onto the repository root".into(),
        }
        .into());
    }

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<StageError>("Failed to deserialize revision state")?;
    // Save the current revision before any modifications — the staged state
    // may share the same Arc<State> and modifications would change both.
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or(state_current);

    // TODO(vri): UCS-18009 - Implement stage move for linked changes
    let from_node = node_outside_links(&state, &repository, from_path.as_str())
        .await
        .forward_with::<StageError, _>(|| format!("looking up the move source {from_path}"))?;
    if !from_node.is_valid_node_id() {
        return Err(StageError::internal(format!(
            "Path {from_path} does not exist in repository "
        )));
    }

    let (is_directory, is_file, added, deleted) = {
        let node = state
            .node(repository.clone(), from_node)
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        (
            node.is_directory(),
            node.is_file(),
            node.is_added(),
            node.is_deleted(),
        )
    };

    if !execution_context().globals().force()
        && repository
            .filter
            .emit_excludes(&to_path, !is_file, FilterMode::Full)
    {
        return Err(StageError::internal(format!("Ignored path {to_path}")));
    }

    let to_node = existing_node_outside_links(&state, &repository, &to_path)
        .await
        .forward_with::<StageError, _>(|| format!("looking up the move target {to_path}"))?;

    let stats = Arc::new(StageStats::default());

    let mut parent_options = options;
    parent_options.no_children = true;

    let parent_link = with_operation(repository.file_system(), async |operation| {
        let to_info = operation
            .file_info(&to_path)
            .await
            .forward::<StageError>("Failed to read the move target")?;
        if !to_info.exists() {
            return Err(StageError::internal(format!(
                "Path {to_path} does not exist in repository "
            )));
        }

        if is_directory && !to_info.is_dir() {
            return Err(StageError::internal("Cannot move a directory to a file"));
        }
        if !is_directory && to_info.is_dir() {
            return Err(StageError::internal("Cannot move a file to a directory"));
        }

        // Make sure the target parent node exist
        let mut parent_path = to_path.clone();
        parent_path.pop();
        let parent_absolute_path =
            parent_path
                .to_absolute_path(repository.require_path()?)
                .forward::<StageError>("placing the move target's parent below the root")?;
        lore_debug!(
            "New parent node path: {}/ ({})",
            parent_path,
            parent_absolute_path.display()
        );

        Box::pin(stage::stage_filesystem_path(
            operation,
            NodeMapping::root(repository.clone(), state.clone()),
            parent_path,
            KnownChild::Unresolved,
            stats.clone(),
            parent_options,
            None, // TODO(vri): UCS-18009 - Implement stage move for linked changes
            None,
            None, // No prefix map for a path resolved on its own
            None, // A move stages the parent alone, so it reaches no boundary
        ))
        .await
    })
    .await?;
    if parent_link.repository != repository.id {
        return Err(StageError::internal(format!(
            "The parent of the move's destination {to_path} lies in a linked repository, past \
             the refusal of a destination in or through a link"
        )));
    }
    let parent = parent_link.node;

    let from_name = from_path.name();
    let to_name = to_path.name();
    let name = if from_name != to_name {
        NameChange::Rename(to_name)
    } else {
        NameChange::Keep
    };
    let (move_flag, dirty_flag) = if added {
        (NodeFlags::StagedAdd, NodeFlags::DirtyAdd)
    } else {
        (NodeFlags::StagedMove, NodeFlags::DirtyMove)
    };
    let own = Mark {
        staged: Some((move_flag, true)),
        dirty: Some((dirty_flag, Ext::Keep, true)),
    };
    let left = state
        .node_move(repository.clone(), from_node, parent, name, own)
        .await
        .forward::<StageError>("Failed to move the node")?;
    // A target the move replaces is staged as deleted once the node has left it, as its deletion
    // reaches every node below it: a directory moved over the one above it was below it until
    // now. A rename of the case alone finds the node itself as its target.
    if let Some(target) = to_node.filter(|target| *target != from_node) {
        lore_debug!("Staging existing target node {} as deleted", target);
        stage::stage_delete(
            repository.clone(),
            state.clone(),
            to_path,
            target,
            options.node_flags,
            stats.clone(),
            None, // TODO(vri): UCS-18009 - Implement stage move for linked changes
        )
        .await?;
    }
    state
        .node_mark_destination(&repository, parent, own)
        .await
        .forward::<StageError>("Failed to mark node as staged")?;

    // If this is a directory move, recursively mark all children as moved
    if is_directory {
        mark_children_moved(&repository, &state, from_node, move_flag, deleted)
            .await
            .forward::<StageError>("Failed to mark node as staged")?;
    }
    let left_committed = left != parent
        && left != ROOT_NODE
        && state
            .node(repository.clone(), left)
            .await
            .forward::<StageError>("Failed deserializing state node block")?
            .stages_when_left();
    if left_committed {
        state
            .node_mark_left(
                &repository,
                (left, parent),
                Mark::staged(NodeFlags::Staged, false),
            )
            .await
            .forward::<StageError>("Failed to stage the directory the node left")?;
    }
    let promoted = stage_promoted_moves(&repository, current_revision, &state).await?;
    stats
        .directory_move_count
        .fetch_add(promoted, Ordering::Relaxed);

    #[allow(clippy::collapsible_else_if)]
    if added {
        if is_directory {
            stats.directory_add_count.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.file_add_count.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        if is_directory {
            stats.directory_move_count.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.file_move_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    // TODO(vri): UCS-18009 - Implement stage move for linked changes
    // Serialize all staged links states recursively

    let count = LoreFileStageCountData::new(stats.clone());
    event::LoreEvent::FileStageEnd(LoreFileStageEndEventData { count }).send();

    state.reparent_onto(current_revision);

    // Serialize new staged state
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged revision state")?;
    if !execution_context().globals().dry_run() {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<StageError>("Failed to serialize staged anchor")?;
    }

    event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
        repository: repository.id,
        revision: signature,
    })
    .send();

    Ok(signature)
}

/// Routing decision for a single stage path against the configured layer set.
///
/// `Inside` routes exclusively to one layer with a possibly-empty `remain` suffix.
/// `AncestorOf` routes to the parent (with the listed layer subtrees masked) AND
/// to every layer whose `mount_path` is under the input path. `Disjoint` routes
/// to the parent only.
///
/// Layer indices refer into the slice passed to [`classify_stage_path`].
#[lore_macro::test_pub]
#[derive(Debug, PartialEq)]
pub(crate) enum LayerRoute {
    Inside {
        layer_index: usize,
        remain: RelativePath,
    },
    AncestorOf {
        layer_indices: Vec<usize>,
    },
    Disjoint,
}

/// Classifies a stage path against a list of layer mount paths.
///
/// Assumes non-overlapping layers (no layer's mount path is a prefix of another's).
#[lore_macro::test_pub]
pub(crate) fn classify_stage_path(relative_path: &str, layer_mount_paths: &[&str]) -> LayerRoute {
    if relative_path.is_empty() {
        return if layer_mount_paths.is_empty() {
            LayerRoute::Disjoint
        } else {
            LayerRoute::AncestorOf {
                layer_indices: (0..layer_mount_paths.len()).collect(),
            }
        };
    }

    for (i, target) in layer_mount_paths.iter().enumerate() {
        if target.is_empty() {
            continue;
        }
        if relative_path == *target {
            return LayerRoute::Inside {
                layer_index: i,
                remain: RelativePath::new(),
            };
        }
        if let Some(rest) = relative_path.strip_prefix(target)
            && rest.starts_with('/')
        {
            return LayerRoute::Inside {
                layer_index: i,
                remain: RelativePath::new_from_clean_parts(&rest[1..], ""),
            };
        }
    }

    let mut ancestor_indices = Vec::new();
    for (i, target) in layer_mount_paths.iter().enumerate() {
        if target.is_empty() {
            continue;
        }
        if let Some(rest) = target.strip_prefix(relative_path)
            && rest.starts_with('/')
        {
            ancestor_indices.push(i);
        }
    }

    if ancestor_indices.is_empty() {
        LayerRoute::Disjoint
    } else {
        LayerRoute::AncestorOf {
            layer_indices: ancestor_indices,
        }
    }
}

/// Splits paths into those the parent repository owns and, per layer, the mount-relative
/// suffixes that layer owns.
///
/// Layer content is deliberately absent from the parent's tree, so a path under a mount
/// evaluated against the parent's states matches nothing at all.
pub(crate) fn route_layer_paths(
    layers: &[crate::layer::Layer],
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

/// Returns true if `relative_path` is at or inside any of the masked subtree paths.
///
/// Used by the parent stage walker to skip layer mount subtrees so files inside
/// layers aren't double-counted on the parent side.
///
/// Generic over `AsRef<str>` so callers can pass either `&[String]`
/// (production: layer target paths) or `&[&str]` (tests). This avoids the
/// per-call Vec<&str> rebuild that the previous `&[&str]`-only signature
/// forced on the production hot path.
#[lore_macro::test_pub]
pub(crate) fn is_path_under_layer_mask<S: AsRef<str>>(relative_path: &str, mask: &[S]) -> bool {
    for entry in mask {
        let entry = entry.as_ref();
        if entry.is_empty() {
            continue;
        }
        if relative_path == entry {
            return true;
        }
        if let Some(rest) = relative_path.strip_prefix(entry)
            && rest.starts_with('/')
        {
            return true;
        }
    }
    false
}

/// The walks of a stage by name, run for tests.
#[cfg(feature = "test-util")]
pub mod test_util {
    use std::sync::Arc;

    use super::StageWalk;
    use crate::fs::filesystem_provider::InstanceOperationImpl;
    use crate::link::LinkTracker;
    use crate::repository::RepositoryContext;
    use crate::stage;
    use crate::stage::StageError;
    use crate::stage::StageOptions;
    use crate::state::NodeMapping;
    use crate::state::State;
    use crate::util::path::RelativePath;

    /// Where the walk of each of `targets` starts, once the directories they share are created
    /// first in `state` with the default options. The queue of discards answers for `state`
    /// rather than the revision the stage was built on, so only walks that leave none, as below a
    /// link, are walked as a stage walks them.
    pub async fn walk_starts(
        operation: Arc<InstanceOperationImpl>,
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        link_tracker: Arc<LinkTracker>,
        targets: &[RelativePath],
    ) -> Result<Vec<NodeMapping>, StageError> {
        let walk = StageWalk {
            operation,
            repository,
            state: state.clone(),
            stats: Arc::default(),
            link_tracker,
            global_mask: None,
            prefixes: None,
            options: StageOptions::default(),
        };
        let shared = super::created_first(&RelativePath::dedup_to_supersets(targets.to_vec()), &[]);
        let discards = Arc::new(stage::DiscardQueue::new(state));
        let ancestors = super::precreate_shared_ancestors(&walk, &shared, &discards).await?;
        Ok(targets
            .iter()
            .map(|target| walk.walk(target.clone(), &ancestors).at)
            .collect())
    }
}
