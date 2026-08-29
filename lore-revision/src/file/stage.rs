// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::event;
use crate::filter::FilterMode;
use crate::hash::hash_string;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::layer;
use crate::link::LinkTracker;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_limit_drain_tasks;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::stage;
use crate::stage::LoreFileStageBeginEventData;
use crate::stage::LoreFileStageCountData;
use crate::stage::LoreFileStageEndEventData;
use crate::stage::LoreFileStageProgressEventData;
use crate::stage::LoreFileStageRevisionEventData;
use crate::stage::StageError;
use crate::stage::StageOptions;
use crate::stage::StageStats;
use crate::state;
use crate::state::State;
use crate::util::path::DepthPath;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
use crate::util::path::path_depth;
use crate::util::path::shared_component_depth;

/// The node of each shared ancestor already created, borrowed from the list they
/// are created from.
type AncestorNodes<'a> = std::collections::HashMap<&'a str, crate::node::NodeID>;

/// The deepest strict ancestor of `path` that has a node, and that node.
///
/// Starts at the parent, never at `path` itself: the caller is about to stage
/// `path`, and starting its walk on top of it would skip it.
fn longest_ancestor<'a>(
    path: &'a str,
    nodes: &AncestorNodes<'_>,
) -> Option<(&'a str, crate::node::NodeID)> {
    let mut end = path.rfind('/')?;
    loop {
        let candidate = &path[..end];
        if let Some(node) = nodes.get(candidate) {
            return Some((candidate, *node));
        }
        end = candidate.rfind('/')?;
    }
}

/// What [`stage::stage_filesystem_path`] takes as the point a walk starts from,
/// and the path it covers below that point.
struct WalkBase {
    absolute: std::path::PathBuf,
    relative: RelativePathBuf,
    node: crate::node::NodeID,
    path: RelativePath,
    /// The prefix map, which is keyed by repository-relative paths and so only
    /// answers for a walk that starts at the repository root.
    prefixes: Option<Arc<crate::util::fs::ResolvedPrefixes>>,
}

impl WalkBase {
    /// The whole of `path` walked from the repository root, for a path with no
    /// pre-created ancestor.
    fn from_root(
        repository_root: &std::path::Path,
        path: RelativePath,
        prefixes: Option<Arc<crate::util::fs::ResolvedPrefixes>>,
    ) -> Self {
        Self {
            absolute: repository_root.to_path_buf(),
            relative: RelativePathBuf::new(),
            node: ROOT_NODE,
            path,
            prefixes,
        }
    }
}

/// Where the walk for `target` should start: the deepest ancestor that already
/// has a node, with what is left of the target below it. `Err` returns `target`
/// untouched, for one with no such ancestor, which has to start at the
/// repository root.
///
/// The chain above the base is resolved once, while it is created, rather than
/// once per target: a metadata syscall and a node lookup per component per
/// target, for an answer that does not change.
///
/// Takes `target` by value so what is left below the base is a view of it
/// rather than a second path built from its bytes.
fn walk_base(
    mut target: RelativePath,
    repository_root: &std::path::Path,
    ancestor_nodes: &AncestorNodes<'_>,
    prefixes: Option<&Arc<crate::util::fs::ResolvedPrefixes>>,
) -> Result<WalkBase, RelativePath> {
    let (absolute, relative, node, prefix_depth) = {
        let Some((prefix, node)) = longest_ancestor(target.as_str(), ancestor_nodes) else {
            return Err(target);
        };
        let prefix_depth = path_depth(prefix);
        // The case the prefix resolved to, and only when it resolved as a whole:
        // a shorter match answers for a shorter path and would drop components.
        let variation = prefixes
            .and_then(|resolved| resolved.longest_prefix_of(prefix))
            .filter(|(depth, _)| *depth == prefix_depth)
            .map_or(prefix, |(_, variation)| variation);
        // Already clean: a map value built from cleaned parts, so it needs no
        // validating or rewriting.
        let relative = RelativePathBuf::new_from_clean_parts(variation, "");
        (
            repository_root.join(variation),
            relative,
            node,
            prefix_depth,
        )
    };
    target.pop_root_repeat(prefix_depth);
    Ok(WalkBase {
        absolute,
        relative,
        node,
        path: target,
        prefixes: None,
    })
}

/// Fold one finished pre-create into the ancestor node map, keeping the first
/// error rather than returning it: the caller drains the level either way.
fn collect_precreate<'a>(
    joined: Result<(usize, Result<crate::node::NodeLink, StageError>), tokio::task::JoinError>,
    ancestors: &'a [DepthPath],
    nodes: &mut AncestorNodes<'a>,
    failure: &mut Option<StageError>,
) {
    match joined {
        Ok((index, Ok(node_link))) => {
            if node_link.is_valid() {
                nodes.insert(ancestors[index].path(), node_link.node);
            }
        }
        Ok((_, Err(err))) => {
            if failure.is_none() {
                *failure = Some(err);
            }
        }
        Err(err) => {
            if failure.is_none() {
                *failure = Some(StageError::internal_with_context(
                    err,
                    "Failed to join pre-create task",
                ));
            }
        }
    }
}

/// The directories two or more of `targets` share, shallowest first and
/// contiguous per depth. Only such a directory is a place where parallel walks
/// would race to create the same node.
///
/// `targets` must be an antichain in lexicographic order, which puts the targets
/// under a directory in one run: a directory is shared exactly when two
/// neighbours agree that far, and emitting it at the first target of its run
/// yields the set once over.
///
/// The result is prefix-closed and holds one case variation of each entry, so a
/// depth is a set of distinct nodes whose parents the depth above holds.
fn shared_ancestors(targets: &[RelativePath]) -> Vec<DepthPath> {
    let mut shared: Vec<DepthPath> = Vec::new();
    let mut preceding = 0;
    for (index, target) in targets.iter().enumerate() {
        let following = targets.get(index + 1).map_or(0, |next| {
            shared_component_depth(target.as_str(), next.as_str())
        });
        let target = target.as_str();
        for (depth, (end, _)) in target.match_indices('/').enumerate() {
            let depth = depth + 1;
            if depth > following {
                break;
            }
            if depth > preceding {
                shared.push(DepthPath::new(target[..end].to_string()));
            }
        }
        preceding = following;
    }
    shared.sort_unstable();
    shared
}

/// Spawn a stage task into the given layer's repository covering `remain` (the
/// path-suffix relative to the layer's mount). An empty `remain` stages the
/// layer's whole subtree.
async fn stage_into_single_layer(
    tasks: &mut JoinSet<Result<crate::node::NodeLink, StageError>>,
    layer: &crate::layer::Layer,
    layer_state: &crate::layer::LayerState,
    parent_repository: Arc<RepositoryContext>,
    remain: RelativePath,
    stats: Arc<StageStats>,
    options: StageOptions,
) -> Result<(), StageError> {
    let absolute_path = parent_repository.require_path()?.join(&layer.target_path);

    let layer_relative_path = RelativePathBuf::new_from_initial_path(&layer.source_path)
        .forward::<StageError>("Failed to construct layer relative path")?;

    // TODO(mjansson): If this has gone past a link into a subrepository, we
    // need to stage the link node and upwards in the layer repository. The base
    // below also stays relative to this repository while the walk runs against
    // the linked one, so the filter and the delete lookup are handed a path the
    // linked repository does not hold.
    let layer_staged_node = layer_state
        .state_staged
        .find_node_link(layer_state.repository.clone(), layer_relative_path.as_str())
        .await
        .forward::<StageError>("Failed to locate layer source base node")?;

    let (layer_repository, layer_state_staged) = layer_staged_node
        .resolve(
            layer_state.repository.clone(),
            layer_state.state_staged.clone(),
        )
        .await
        .forward::<StageError>("Failed to locate layer source base node")?;

    lore_debug!(
        "Staging path in layer {}: {} / {}",
        layer.target_path,
        layer.source_path,
        remain
    );

    lore_spawn!(
        tasks,
        stage::stage_filesystem_path(
            layer_repository,
            layer_state_staged,
            absolute_path,
            layer_relative_path,
            layer_staged_node.node,
            remain,
            stats,
            options,
            None, // No link tracking in layer staging
            None, // Layers don't have nested layer mounts (no overlap)
            None, // Prefixes are resolved against the repository root, not a layer
        )
    );

    Ok(())
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
///   calls to keep dirty flags accurate. When [`StageOptions::scan`] is
///   `true`, the directory is walked recursively on the filesystem, every
///   contained file is reconciled, and the dirty flags are disregarded.
pub async fn stage(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    options: StageOptions,
) -> Result<Hash, StageError> {
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

    // Every layer mount is staged by its own task, never the parent walk, so
    // masking every layer subtree on every main-repo walk is correct: an entry
    // not under a given target is never reached anyway.
    let global_mask: Option<Arc<Vec<String>>> = if layers.is_empty() {
        None
    } else {
        Some(Arc::new(
            layers
                .iter()
                .map(|(layer, _)| layer.target_path.clone())
                .collect(),
        ))
    };
    let layer_target_refs: Vec<&str> = global_mask
        .as_deref()
        .map(|paths| paths.iter().map(String::as_str).collect())
        .unwrap_or_default();

    let RoutedTargets {
        current_repository_paths,
        layer_paths,
    } = route_and_resolve_targets(&repository, &state, &paths, &layer_target_refs, options).await?;

    // A root target covers the whole tree; otherwise collapse overlaps so a
    // parent target subsumes anything that would be staged beneath it.
    let stage_root = current_repository_paths.iter().any(|p| p.is_empty());
    let antichain: Vec<RelativePath> = if stage_root {
        vec![RelativePath::new()]
    } else {
        RelativePath::dedup_to_supersets(current_repository_paths)
    };
    let antichain_len = antichain.len();

    let shared_ancestors = shared_ancestors(&antichain);
    let precreate_count = shared_ancestors.len();

    // Resolve the case of those directories once, so no target under them
    // resolves them again. Not under `Keep`, which stages by renaming the file
    // system to match the tree - the first such rename would leave the map
    // naming a directory that is no longer there.
    let prefixes = if matches!(options.case_change, stage::StageCaseChange::Keep) {
        None
    } else {
        let prefixes = Arc::new(
            crate::util::fs::resolve_prefixes(repository.require_path()?, &shared_ancestors).await,
        );
        lore_debug!(
            "Resolved {} of {} shared ancestor prefixes",
            prefixes.len(),
            precreate_count
        );
        Some(prefixes)
    };

    let mut precreate_options = options;
    precreate_options.no_children = true;
    let repository_root = repository.require_path()?.to_path_buf();
    // Node of each shared ancestor as it is created, so the next level starts
    // from its parent instead of resolving the chain above it again.
    let mut ancestor_nodes = AncestorNodes::with_capacity(shared_ancestors.len());
    let mut precreate_failure: Option<StageError> = None;

    for level in shared_ancestors.chunk_by(|left, right| left.depth() == right.depth()) {
        if precreate_failure.is_some() {
            break;
        }

        let mut level_tasks: JoinSet<(usize, Result<crate::node::NodeLink, StageError>)> =
            JoinSet::new();
        for (index, ancestor) in level.iter().enumerate() {
            if precreate_failure.is_some() {
                break;
            }
            let ancestor = RelativePath::new_from_clean_parts(ancestor.path(), "");
            let base = walk_base(
                ancestor,
                repository_root.as_path(),
                &ancestor_nodes,
                prefixes.as_ref(),
            );
            let base = base.unwrap_or_else(|ancestor| {
                WalkBase::from_root(repository_root.as_path(), ancestor, prefixes.clone())
            });
            let repository = repository.clone();
            let state = state.clone();
            let stats = stats.clone();
            let link_tracker = link_tracker.clone();
            let global_mask = global_mask.clone();
            lore_spawn!(level_tasks, async move {
                let result = stage::stage_filesystem_path(
                    repository,
                    state,
                    base.absolute,
                    base.relative,
                    base.node,
                    base.path,
                    stats,
                    precreate_options,
                    Some(link_tracker),
                    global_mask,
                    base.prefixes,
                )
                .await;
                (index, result)
            });

            while let Some(joined) = level_tasks.try_join_next() {
                collect_precreate(joined, level, &mut ancestor_nodes, &mut precreate_failure);
            }
            while level_tasks.len() >= MAX_CONCURRENT_TREE_TASKS
                && let Some(joined) = level_tasks.join_next().await
            {
                collect_precreate(joined, level, &mut ancestor_nodes, &mut precreate_failure);
            }
        }
        // Drained even on failure: a pre-create in flight is allocating nodes and
        // must not be cancelled part way through.
        while let Some(joined) = level_tasks.join_next().await {
            collect_precreate(joined, level, &mut ancestor_nodes, &mut precreate_failure);
        }
    }
    if let Some(err) = precreate_failure {
        return Err(err);
    }

    // Shared ancestors now exist and the targets are disjoint, so every remaining
    // creation is single-writer or a distinct sibling, which `node_add` publishes
    // with an atomic CAS prepend. Layer jobs run against their own separate states.
    let mut failure = None;
    let mut tasks: JoinSet<Result<crate::node::NodeLink, StageError>> = JoinSet::new();

    for target in antichain {
        let base = walk_base(
            target,
            repository_root.as_path(),
            &ancestor_nodes,
            prefixes.as_ref(),
        );
        let base = base.unwrap_or_else(|target| {
            WalkBase::from_root(repository_root.as_path(), target, prefixes.clone())
        });
        lore_spawn!(
            tasks,
            stage::stage_filesystem_path(
                repository.clone(),
                state.clone(),
                base.absolute,
                base.relative,
                base.node,
                base.path,
                stats.clone(),
                options,
                Some(link_tracker.clone()),
                global_mask.clone(),
                base.prefixes,
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
    let main_count = antichain_len + precreate_count;

    // A layer may be targeted by several paths; serialize each only once.
    let staged_layers: std::collections::BTreeSet<usize> = layer_paths
        .iter()
        .map(|(layer_index, _)| *layer_index)
        .collect();

    if failure.is_none() {
        for (layer_index, remain) in layer_paths {
            let (layer, layer_state) = &layers[layer_index];
            if let Err(err) = stage_into_single_layer(
                &mut tasks,
                layer,
                layer_state,
                repository.clone(),
                remain,
                stats.clone(),
                options,
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
    }

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
    if let Some(err) = failure {
        return Err(err);
    }

    let layer_staged: Vec<_> = staged_layers
        .into_iter()
        .map(|layer_index| (&layers[layer_index].0, &layers[layer_index].1))
        .collect();

    let count = LoreFileStageCountData::new(stats.clone());
    let total_count = count.total_count;
    event::LoreEvent::FileStageEnd(LoreFileStageEndEventData { count }).send();

    if total_count == 0 {
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
    let parent_mutated = main_count > 0 && (state.is_dirty() || link_tracker.has_modifications());
    if parent_mutated {
        // Process links that need reserialization due to downstream changes
        stage::process_link_updates(
            repository.clone(),
            token,
            state_current.clone(),
            state.clone(),
            link_tracker.clone(),
        )
        .await?;

        // Staged states should have no revision number
        state.set_revision_number(0);

        state.set_parent_self(current_revision);

        // If staged state is the initial stage based on current state, reset other parent. Otherwise
        // leave it as is, in case previous staged state was a merge/integrate
        if staged_revision == current_revision {
            state.set_parent_other(Hash::default());
            state.set_metadata_hash(Hash::default());
        }

        let signature = state
            .serialize(repository.clone(), token)
            .await
            .forward::<StageError>("Failed to serialize staged revision state")?;

        if signature != current_revision {
            staged_revision = signature;
            crate::instance::store_staged_anchor(&repository, signature)
                .await
                .forward::<StageError>("Failed to serialize staged anchor")?;
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

        state.set_revision_number(0);

        state.set_parent_self(layer_state.state_current.revision());

        // If staged state is the initial stage based on current state, reset other parent. Otherwise
        // leave it as is, in case previous staged state was a merge/integrate
        if layer_state.state_current.revision() == layer_state.state_staged.revision() {
            state.set_parent_other(Hash::default());
            state.set_metadata_hash(Hash::default());
        }

        let signature = state
            .serialize(layer_state.repository.clone(), token)
            .await
            .forward::<StageError>("Failed to serialize staged revision state")?;

        if signature != layer.current {
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

/// What a stage target list resolved to.
struct RoutedTargets {
    /// Paths to walk in the current repository.
    current_repository_paths: Vec<RelativePath>,
    /// Layer index, and the path relative to that layer's mount. An empty path
    /// stages the layer's whole subtree.
    ///
    /// Ordered by input, so the failure reported when several layers fail is the
    /// one belonging to the earliest target given.
    layer_paths: Vec<(usize, RelativePath)>,
}

/// Route each of `paths` through the configured layers and resolve what it
/// stages.
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
    state: &Arc<State>,
    paths: &LoreArray<LoreString>,
    layer_target_refs: &[&str],
    options: StageOptions,
) -> Result<RoutedTargets, StageError> {
    let mut routed = RoutedTargets {
        current_repository_paths: Vec::with_capacity(paths.len()),
        layer_paths: Vec::new(),
    };
    let mut resolve_tasks: JoinSet<Result<ResolvedTarget, StageError>> = JoinSet::new();
    let mut failure: Option<StageError> = None;

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

        let task_repository = repository.clone();
        let task_state = state.clone();
        lore_spawn!(resolve_tasks, async move {
            resolve_stage_target(task_repository, task_state, relative_path, options).await
        });
        while let Some(joined) = resolve_tasks.try_join_next() {
            collect_resolved(joined, &mut routed.current_repository_paths, &mut failure);
        }
        while resolve_tasks.len() >= MAX_CONCURRENT_TREE_TASKS
            && let Some(joined) = resolve_tasks.join_next().await
        {
            collect_resolved(joined, &mut routed.current_repository_paths, &mut failure);
        }
    }

    while let Some(joined) = resolve_tasks.join_next().await {
        collect_resolved(joined, &mut routed.current_repository_paths, &mut failure);
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(routed),
    }
}

/// What one stage target resolves to.
///
/// A target that is not a directory being descended - a single file, a `scan`
/// target, a path with no node - resolves to itself, and a targets file is
/// mostly those. Naming that case keeps the common result off the heap.
enum ResolvedTarget {
    Single(RelativePath),
    Multiple(Vec<RelativePath>),
}

impl ResolvedTarget {
    fn collect_into(self, targets: &mut Vec<RelativePath>) {
        match self {
            ResolvedTarget::Single(path) => targets.push(path),
            ResolvedTarget::Multiple(paths) => targets.extend(paths),
        }
    }
}

/// Fold one finished target resolution into the target list, keeping the first
/// error rather than propagating it - the caller has to drain the rest either
/// way, since a resolution in flight is reading state it has to finish reading.
fn collect_resolved(
    joined: Result<Result<ResolvedTarget, StageError>, tokio::task::JoinError>,
    targets: &mut Vec<RelativePath>,
    failure: &mut Option<StageError>,
) {
    match joined {
        Ok(Ok(resolved)) => resolved.collect_into(targets),
        Ok(Err(err)) => {
            if failure.is_none() {
                *failure = Some(err);
            }
        }
        Err(err) => {
            if failure.is_none() {
                *failure = Some(StageError::internal_with_context(
                    err,
                    "Failed to join target resolution task",
                ));
            }
        }
    }
}

/// Resolve `relative_path` to the concrete set of repository-relative paths to
/// stage. Without `scan`, a directory resolves to its dirty descendants (empty
/// when none); `scan`, single files, and paths with no node resolve to the path
/// itself.
///
/// `find_node_link` follows link mounts transparently — a crossed link is read
/// from the state that owns it, otherwise a colliding block at the same
/// coordinates in the parent state would misclassify the target. The returned
/// paths stay parent-relative, since the filesystem walk traverses links itself.
async fn resolve_stage_target(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    options: StageOptions,
) -> Result<ResolvedTarget, StageError> {
    if !options.scan {
        let resolved: Option<(
            Arc<State>,
            Arc<RepositoryContext>,
            crate::node::NodeID,
            bool,
        )> = if relative_path.is_empty() {
            // Root path is always a directory in the main repository.
            Some((state.clone(), repository.clone(), ROOT_NODE, true))
        } else if let Ok(node_link) = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            && node_link.is_valid()
        {
            let (resolved_repository, resolved_state) = if node_link.repository == repository.id {
                (repository.clone(), state.clone())
            } else {
                let linked_repository =
                    Arc::new(repository.to_link_context(node_link.repository).await);
                let linked_state =
                    State::deserialize(linked_repository.clone(), node_link.revision)
                        .await
                        .forward::<StageError>(
                            "Failed to deserialize linked state for dirty staging",
                        )?;
                (linked_repository, linked_state)
            };
            let node = resolved_state
                .node(resolved_repository.clone(), node_link.node)
                .await
                .forward::<StageError>("Failed to resolve node for dirty staging")?;
            Some((
                resolved_state,
                resolved_repository,
                node_link.node,
                node.is_directory(),
            ))
        } else {
            None
        };

        if let Some((resolved_state, resolved_repository, root_node, true)) = resolved {
            let dirty_paths = resolved_state
                .collect_dirty_paths(
                    resolved_repository,
                    root_node,
                    RelativePathBuf::new_from_clean_parts(relative_path.as_str(), ""),
                )
                .await
                .forward::<StageError>("Failed to collect dirty paths")?;
            return Ok(ResolvedTarget::Multiple(dirty_paths));
        }
    }

    Ok(ResolvedTarget::Single(relative_path))
}

/// Recursively mark all children of a directory node as moved.
/// This is called when a directory is moved to ensure all contained files
/// and subdirectories also have the move flag set.
async fn mark_children_moved(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent_node: crate::node::NodeID,
    move_flag: NodeFlags,
) -> Result<(), crate::state::StateError> {
    use std::future::Future;
    use std::pin::Pin;

    fn mark_children_moved_recursive(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        parent_node: crate::node::NodeID,
        move_flag: NodeFlags,
    ) -> Pin<Box<dyn Future<Output = Result<(), crate::state::StateError>> + Send>> {
        Box::pin(async move {
            let children = state.node_children(repository.clone(), parent_node).await?;

            for child_id in children {
                let child_node = state.node(repository.clone(), child_id).await?;

                // Determine the appropriate flag for this child
                let child_flag = if child_node.is_staged_add() {
                    NodeFlags::StagedAdd
                } else {
                    move_flag
                };

                // Mark the child node
                state
                    .node_mark(repository.clone(), child_id, child_flag, false)
                    .await?;

                // Recurse into directories
                if child_node.is_directory() {
                    mark_children_moved_recursive(
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

    mark_children_moved_recursive(repository, state, parent_node, move_flag).await
}

#[allow(clippy::too_many_arguments)]
pub async fn stage_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    options: StageOptions,
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

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    let stats = Arc::new(StageStats::default());
    for path in paths.as_slice() {
        let Some(relative_path) = normalize_stage_path(&repository, path).await else {
            continue;
        };

        // TODO(mjansson): Layers

        lore_debug!("Stage merge options: {:?}", options);
        let mut task = lore_spawn!(stage::stage_merge_path(
            repository.clone(),
            state_stage.clone(),
            state_merge.clone(),
            relative_path.clone(),
            stats.clone(),
            options,
            None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
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

    // TODO(vri): UCS-17955 - Merging and conflict resolution for links
    // Serialize all staged links states recursively

    let signature = state_stage
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged revision state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<StageError>("Failed to serialize staged anchor")?;

    event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
        repository: repository.id,
        revision: signature,
    })
    .send();

    Ok(signature)
}

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

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<StageError>("Failed to deserialize revision state")?;
    // Save the current revision before any modifications — the staged state
    // may share the same Arc<State> and modifications would change both.
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or(state_current);

    if !execution_context().globals().force()
        && repository
            .filter
            .emit_excludes(&to_path, true, FilterMode::Full)
    {
        return Err(StageError::internal(format!("Ignored path {to_path}")));
    }

    // Find from node (must exist, optionally already staged for delete)
    let from_node_link = state
        .find_node_link(repository.clone(), from_path.as_str())
        .await
        .forward_with::<StageError, _>(|| {
            format!("Path {from_path} does not exist in repository ")
        })?;
    if !from_node_link.is_valid() {
        return Err(StageError::internal(format!(
            "Path {from_path} does not exist in repository "
        )));
    }

    let from_node = state
        .node(repository.clone(), from_node_link.node)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;

    // Find to node (optional)
    let to_node_link = state
        .find_node_link(repository.clone(), to_path.as_str())
        .await
        .unwrap_or_default();

    // Get target file/directory metadata
    let to_absolute_path = to_path.to_absolute_path(repository.require_path()?);
    let to_metadata = lore_io::IoDriver::global()
        .metadata(to_absolute_path)
        .await
        .internal_with(|| format!("Path {to_path} does not exist in repository "))?;

    if from_node.is_directory() && !to_metadata.is_dir() {
        return Err(StageError::internal("Cannot move a directory to a file"));
    }
    if !from_node.is_directory() && to_metadata.is_dir() {
        return Err(StageError::internal("Cannot move a file to a directory"));
    }

    let stats = Arc::new(StageStats::default());

    if to_node_link.is_valid() {
        // Stage existing target node as deleted, it is being replaced by the source file
        lore_debug!(
            "Staging existing target node {} as deleted",
            to_node_link.node
        );
        if to_node_link.repository != repository.id {
            // TODO(vri): UCS-18009 - Implement stage move for linked changes
            return Err(StageError::internal(
                "Links not yet implemented, cannot perform actions in other repositories",
            ));
        }

        stage::stage_delete(
            repository.clone(),
            state.clone(),
            to_node_link.node,
            options.node_flags,
            stats.clone(),
            None, // TODO(vri): UCS-18009 - Implement stage move for linked changes
        )
        .await?;
    }

    // Make sure the target parent node exist
    let mut parent_path = to_path.clone();
    parent_path.pop();
    let parent_absolute_path = parent_path.to_absolute_path(repository.require_path()?);
    lore_debug!(
        "New parent node path: {}/ ({})",
        parent_path,
        parent_absolute_path.display()
    );

    let mut parent_options = options;
    parent_options.no_children = true;

    let parent_node_link = Box::pin(stage::stage_filesystem_path(
        repository.clone(),
        state.clone(),
        repository.require_path()?.to_path_buf(),
        RelativePathBuf::new(),
        ROOT_NODE,
        parent_path,
        stats.clone(),
        parent_options,
        None, // TODO(vri): UCS-18009 - Implement stage move for linked changes
        None,
        None, // No prefix map for a path resolved on its own
    ))
    .await?;

    let block_index = NodeBlock::index(from_node_link.node);
    let node_index = Node::index(from_node_link.node);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;
    let mut node = block.node(node_index);

    if node.parent != parent_node_link.node {
        // Unlink it from the previous parent child list
        lore_debug!(
            "Unlink node {} from previous parent node: {}",
            from_node_link.node,
            node.parent
        );
        let parent_block_index = NodeBlock::index(node.parent);
        let parent_node_index = Node::index(node.parent);
        let parent_block = state
            .block(repository.clone(), parent_block_index)
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        let parent_node = parent_block.node(parent_node_index);
        if parent_node.child == from_node_link.node {
            lore_debug!(
                "Parent {} child node match, new child node: {}",
                node.parent,
                node.sibling
            );
            let dirtied = {
                let mut block_writer = parent_block.write();
                block_writer.node(parent_node_index).child = node.sibling;
                block_writer.mark_dirty()
            };
            if dirtied {
                state.block_modified(parent_block, parent_block_index);
                state.mark_dirty();
            }
        } else {
            lore_debug!(
                "Parent {} child node does not match, find in sibling list",
                node.parent
            );
            let mut found = false;
            let parent_id = node.parent;
            let mut child_id = parent_node.child().unwrap_or_default();
            let mut cycle = SiblingCycleGuard::new(parent_id);
            while let Some(sibling) = {
                let child = state
                    .node(repository.clone(), child_id)
                    .await
                    .forward::<StageError>("Failed deserializing state node block")?;
                child
                    .walk_step(child_id, parent_id, &mut cycle)
                    .forward::<StageError>("Invalid node hierarchy in stage walk")?;
                child.sibling()
            } {
                if sibling == from_node_link.node {
                    lore_debug!(
                        "Node {} sibling match, replace with new sibling {}",
                        child_id,
                        node.sibling
                    );
                    let child_block_index = NodeBlock::index(child_id);
                    let child_node_index = Node::index(child_id);
                    let child_block =
                        state
                            .block(repository.clone(), child_block_index)
                            .await
                            .forward::<StageError>("Failed deserializing state node block")?;
                    let dirtied = {
                        let mut block_writer = child_block.write();
                        block_writer.node(child_node_index).sibling = node.sibling;
                        block_writer.mark_dirty()
                    };
                    if dirtied {
                        state.block_modified(child_block, child_block_index);
                        state.mark_dirty();
                    }
                    found = true;
                    break;
                }
                lore_debug!(
                    "Node {} sibling does not match, move to {}",
                    child_id,
                    sibling
                );
                child_id = sibling;
            }
            if !found {
                return Err(StageError::internal(
                    "Node not found in child node list, inconsistent repository state",
                ));
            }
        }

        // Inject it into the new parent child list
        lore_debug!(
            "Link node {} to new parent node {} child list",
            from_node_link.node,
            parent_node_link.node
        );
        let parent_block_index = NodeBlock::index(parent_node_link.node);
        let parent_node_index = Node::index(parent_node_link.node);
        let parent_block = state
            .block(repository.clone(), parent_block_index)
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        let sibling_node_id = parent_block.node(parent_node_index).child;
        let dirtied = {
            let mut block_writer = parent_block.write();
            block_writer.node(parent_node_index).child = from_node_link.node;
            block_writer.mark_dirty()
        };
        if dirtied {
            state.block_modified(parent_block, parent_block_index);
            state.mark_dirty();
        }

        lore_debug!(
            "Update node {} sibling node to {}",
            from_node_link.node,
            sibling_node_id
        );
        node.sibling = sibling_node_id;
    }

    // Set the new node metadata - parent node and name (sibling node set above)
    {
        lore_debug!(
            "Update node {} parent to node {}",
            from_node_link.node,
            parent_node_link.node
        );
        node.parent = parent_node_link.node;

        let from_name = from_path.name();
        let to_name = to_path.name();
        if from_name != to_name {
            // Rename the from node
            block
                .deserialize_nametable(repository.clone())
                .await
                .forward::<StageError>("Failed deserializing name table")?;
            lore_debug!(
                "Rename node {}: {} -> {}",
                from_node_link.node,
                from_name,
                to_name
            );
            node.name_hash = hash_string(to_name);
            (node.name_offset, node.name_length) = block
                .write()
                .node_name_store(to_name, node.name_offset, node.name_length)
                .forward::<StageError>("Storing renamed node name")?;
        }
    }

    let dirtied = {
        let mut block_writer = block.write();
        *block_writer.node(node_index) = node;
        block_writer.mark_dirty()
    };
    if dirtied {
        state.block_modified(block, block_index);
        state.mark_dirty();
    }

    // Mark from node as moved
    let move_flag = if from_node.is_staged_add() {
        NodeFlags::StagedAdd
    } else {
        NodeFlags::StagedMove
    };
    state
        .node_mark(
            repository.clone(),
            from_node_link.node,
            move_flag,
            true, /* Mark dirty */
        )
        .await
        .forward::<StageError>("Failed to mark node as staged")?;

    // If this is a directory move, recursively mark all children as moved
    if from_node.is_directory() {
        mark_children_moved(
            repository.clone(),
            state.clone(),
            from_node_link.node,
            move_flag,
        )
        .await
        .forward::<StageError>("Failed to mark node as staged")?;
    }

    #[allow(clippy::collapsible_else_if)]
    if from_node.is_staged_add() {
        if from_node.is_directory() {
            stats.directory_add_count.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.file_add_count.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        if from_node.is_directory() {
            stats.directory_move_count.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.file_move_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    // TODO(vri): UCS-18009 - Implement stage move for linked changes
    // Serialize all staged links states recursively

    let count = LoreFileStageCountData::new(stats.clone());
    event::LoreEvent::FileStageEnd(LoreFileStageEndEventData { count }).send();

    state.set_parent_self(current_revision);

    // If staged state is the initial stage based on current state, reset other parent. Otherwise
    // leave it as is, in case previous staged state was a merge/integrate
    if state.revision() == current_revision {
        state.set_parent_other(Hash::default());
        state.set_metadata_hash(Hash::default());
    }

    // Serialize new staged state
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged revision state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<StageError>("Failed to serialize staged anchor")?;

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
/// to every layer whose `target_path` is under the input path. `Disjoint` routes
/// to the parent only.
///
/// Layer indices refer into the slice passed to [`classify_stage_path`].
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

/// Classifies a stage path against a list of layer mount paths (`target_path`s).
///
/// Assumes non-overlapping layers (no layer's `target_path` is a prefix of another's).
pub(crate) fn classify_stage_path(relative_path: &str, layer_target_paths: &[&str]) -> LayerRoute {
    if relative_path.is_empty() {
        return if layer_target_paths.is_empty() {
            LayerRoute::Disjoint
        } else {
            LayerRoute::AncestorOf {
                layer_indices: (0..layer_target_paths.len()).collect(),
            }
        };
    }

    for (i, target) in layer_target_paths.iter().enumerate() {
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
    for (i, target) in layer_target_paths.iter().enumerate() {
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

/// Returns true if `relative_path` is at or inside any of the masked subtree paths.
///
/// Used by the parent stage walker to skip layer mount subtrees so files inside
/// layers aren't double-counted on the parent side.
///
/// Generic over `AsRef<str>` so callers can pass either `&[String]`
/// (production: layer target paths) or `&[&str]` (tests). This avoids the
/// per-call Vec<&str> rebuild that the previous `&[&str]`-only signature
/// forced on the production hot path.
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

#[cfg(test)]
mod mask_tests {
    use super::*;

    #[test]
    fn empty_mask_never_masks() {
        let empty: [&str; 0] = [];
        assert!(!is_path_under_layer_mask("external/lib", &empty));
        assert!(!is_path_under_layer_mask("", &empty));
    }

    #[test]
    fn exact_mask_match_is_masked() {
        assert!(is_path_under_layer_mask("external/lib", &["external/lib"]));
    }

    #[test]
    fn path_inside_masked_subtree_is_masked() {
        assert!(is_path_under_layer_mask(
            "external/lib/src/foo.rs",
            &["external/lib"]
        ));
    }

    #[test]
    fn ancestor_of_masked_path_is_not_masked() {
        // Walker entering "external" should still descend; the mask kicks in
        // when it reaches "external/lib".
        assert!(!is_path_under_layer_mask("external", &["external/lib"]));
    }

    #[test]
    fn disjoint_path_is_not_masked() {
        assert!(!is_path_under_layer_mask("src/main.rs", &["external/lib"]));
    }

    #[test]
    fn empty_path_with_mask_is_not_masked() {
        // The parent's root is never itself masked.
        assert!(!is_path_under_layer_mask("", &["external/lib"]));
    }

    #[test]
    fn prefix_string_match_without_separator_is_not_masked() {
        assert!(!is_path_under_layer_mask(
            "external_other/file.rs",
            &["external"]
        ));
    }

    #[test]
    fn multiple_mask_entries_any_match_is_masked() {
        let mask = ["external/lib", "vendor/foo"];
        assert!(is_path_under_layer_mask("vendor/foo/x.rs", &mask));
        assert!(is_path_under_layer_mask("external/lib", &mask));
        assert!(!is_path_under_layer_mask("src/main.rs", &mask));
    }
}

#[cfg(test)]
mod shared_ancestor_tests {
    use super::*;

    fn antichain(targets: &[&str]) -> Vec<RelativePath> {
        RelativePath::dedup_to_supersets(
            targets
                .iter()
                .map(|path| RelativePath::new_from_initial_path(path).expect("Path init failed"))
                .collect(),
        )
    }

    fn derived(targets: &[&str]) -> Vec<String> {
        shared_ancestors(&antichain(targets))
            .iter()
            .map(|ancestor| ancestor.path().to_string())
            .collect()
    }

    /// Every ancestor of every target, counted. What the scan over neighbours
    /// arrives at without the counting.
    fn counted(targets: &[RelativePath]) -> Vec<String> {
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for target in targets {
            let mut ancestor = target.clone();
            ancestor.pop();
            while !ancestor.is_empty() {
                *counts.entry(ancestor.as_str().to_string()).or_insert(0) += 1;
                ancestor.pop();
            }
        }
        let mut shared: Vec<String> = counts
            .into_iter()
            .filter_map(|(path, count)| (count >= 2).then_some(path))
            .collect();
        shared.sort_unstable_by(|a, b| path_depth(a).cmp(&path_depth(b)).then_with(|| a.cmp(b)));
        shared
    }

    #[test]
    fn only_a_directory_two_targets_share_is_returned() {
        assert!(derived(&[]).is_empty());
        assert!(derived(&["a/b/c"]).is_empty());
        assert!(derived(&["a/x", "b/y"]).is_empty());
        assert_eq!(derived(&["a/x", "a/y"]), vec!["a"]);
    }

    #[test]
    fn the_result_is_prefix_closed() {
        assert_eq!(derived(&["a/b/c/x", "a/b/c/y"]), vec!["a", "a/b", "a/b/c"]);
    }

    #[test]
    fn one_depth_is_a_contiguous_range_shallowest_first() {
        let shared = shared_ancestors(&antichain(&[
            "a/p/x", "a/p/y", "a/q/x", "a/q/y", "b/p/x", "b/p/y",
        ]));
        let paths: Vec<&str> = shared.iter().map(DepthPath::path).collect();
        assert_eq!(paths, vec!["a", "b", "a/p", "a/q", "b/p"]);
        let depths: Vec<usize> = shared.iter().map(DepthPath::depth).collect();
        assert!(depths.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    /// Two case variations of one directory would be two ancestors naming one
    /// node, and the depth holding both would add it twice. The target set they
    /// are taken from settles on one, and that is what carries into here.
    #[test]
    fn targets_on_one_case_variation_give_ancestors_on_one() {
        assert_eq!(
            derived(&["Assets/Meshes/a", "assets/meshes/b", "ASSETS/Meshes/c"]),
            vec!["Assets", "Assets/Meshes"]
        );
    }

    /// The scan reads neighbours, so the shapes that matter are the ones where
    /// lexicographic order puts something unrelated between two targets, or
    /// where a shared string prefix stops inside a component.
    #[test]
    fn it_agrees_with_counting_every_ancestor() {
        for targets in [
            &[][..],
            &["a/b/c"],
            &["a/x", "a/y"],
            &["a/x", "b/y"],
            &["a/b/c/d/x", "a/b/c/d/y"],
            &["a/p/x", "a/p/y", "a/q/x", "a/q/y", "b/p/x", "b/p/y"],
            // '-' sorts below '/', so this lands between "a" and its subtree.
            &["a/x", "a-foo/y", "a/z"],
            // '0' sorts above '/', so this lands after the subtree.
            &["a/x", "a0/y", "a/z"],
            // A shared string prefix that stops inside a component.
            &["a/b/x", "a/bc/y", "a/b/z"],
            // A directory target covering the files beneath it.
            &["a/b", "a/b/x", "a/b/y", "a/c/x", "a/c/y"],
            // Three levels, and a lone target beside them.
            &["t/m/l/f", "t/m/l/g", "t/m/n/f", "t/m/n/g", "u/v/w/x"],
        ] {
            let antichain = antichain(targets);
            let derived: Vec<String> = shared_ancestors(&antichain)
                .iter()
                .map(|ancestor| ancestor.path().to_string())
                .collect();
            assert_eq!(derived, counted(&antichain), "{targets:?}");
        }
    }
}

#[cfg(test)]
mod walk_base_tests {
    use super::*;

    const NODE_A: crate::node::NodeID = 11;
    const NODE_AB: crate::node::NodeID = 22;

    fn created<'a>(entries: &[(&'a str, crate::node::NodeID)]) -> AncestorNodes<'a> {
        entries.iter().copied().collect()
    }

    fn resolved(entries: &[(&str, &str)]) -> Arc<crate::util::fs::ResolvedPrefixes> {
        let mut prefixes = crate::util::fs::ResolvedPrefixes::default();
        for (path, variation) in entries {
            prefixes.insert((*path).to_string(), (*variation).to_string());
        }
        Arc::new(prefixes)
    }

    #[test]
    fn longest_ancestor_takes_the_deepest_one_created() {
        let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);
        assert_eq!(longest_ancestor("a/b/c/d", &nodes), Some(("a/b", NODE_AB)));
        // In the map, but a walk about to stage it has to start above it.
        assert_eq!(longest_ancestor("a/b", &nodes), Some(("a", NODE_A)));
        assert_eq!(longest_ancestor("a", &nodes), None);
        assert_eq!(longest_ancestor("x/y", &nodes), None);
    }

    fn path(path: &str) -> RelativePath {
        RelativePath::new_from_clean_parts(path, "")
    }

    #[test]
    fn walk_base_starts_at_that_ancestor_with_the_rest_below_it() {
        let root = std::path::Path::new("/repo");
        let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);

        let base = walk_base(path("a/b/c"), root, &nodes, None).expect("an ancestor was created");
        assert_eq!(base.absolute, root.join("a/b"));
        assert_eq!(base.relative.as_str(), "a/b");
        assert_eq!(base.node, NODE_AB);
        assert_eq!(base.path.as_str(), "c");
        assert!(
            base.prefixes.is_none(),
            "the map answers from the root only"
        );

        let base = walk_base(path("a/x/y/z"), root, &nodes, None).expect("an ancestor was created");
        assert_eq!(base.relative.as_str(), "a");
        assert_eq!(base.node, NODE_A);
        assert_eq!(base.path.as_str(), "x/y/z");
    }

    /// The remainder is a view of the target, so its lowercase form has to be
    /// advanced along with it rather than left naming the whole path.
    #[test]
    fn walk_base_leaves_the_remainder_lowercased_from_the_base_down() {
        let root = std::path::Path::new("/repo");
        let nodes = created(&[("Assets", NODE_A)]);

        let base =
            walk_base(path("Assets/Meshes/Rock"), root, &nodes, None).expect("one was created");
        assert_eq!(base.path.as_str(), "Meshes/Rock");
        assert_eq!(base.path.as_lowercase_str(), "meshes/rock");
    }

    #[test]
    fn walk_base_gives_the_target_back_when_no_ancestor_was_created() {
        let root = std::path::Path::new("/repo");
        let nodes = created(&[("x", NODE_A)]);

        let Err(returned) = walk_base(path("a/b"), root, &nodes, None) else {
            panic!("no ancestor was created");
        };
        assert_eq!(returned.as_str(), "a/b", "the target comes back untouched");

        assert!(walk_base(path("a"), root, &nodes, None).is_err());
        assert!(walk_base(path("a"), root, &created(&[]), None).is_err());
    }

    #[test]
    fn from_root_covers_the_whole_path_and_keeps_the_prefix_map() {
        let root = std::path::Path::new("/repo");
        let prefixes = resolved(&[("a", "A")]);

        let base = WalkBase::from_root(
            root,
            RelativePath::new_from_clean_parts("a/b", ""),
            Some(prefixes),
        );
        assert_eq!(base.absolute, root);
        assert!(base.relative.is_empty());
        assert_eq!(base.node, ROOT_NODE);
        assert_eq!(base.path.as_str(), "a/b");
        assert!(base.prefixes.is_some(), "a walk from the root can use it");
    }

    #[test]
    fn walk_base_takes_the_case_the_prefix_resolved_to() {
        let root = std::path::Path::new("/repo");
        let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);
        let prefixes = resolved(&[("a", "A"), ("a/b", "A/B")]);

        let base = walk_base(path("a/b/c"), root, &nodes, Some(&prefixes))
            .expect("an ancestor was created");
        assert_eq!(base.absolute, root.join("A/B"));
        assert_eq!(base.relative.as_str(), "A/B");
        assert_eq!(base.node, NODE_AB);
        assert_eq!(base.path.as_str(), "c", "the remainder is not recased");
    }

    /// A prefix resolves as a whole or not at all: the map answers for the
    /// longest prefix it holds, and a shorter one answers for a shorter path.
    #[test]
    fn walk_base_ignores_a_resolution_covering_only_part_of_the_prefix() {
        let root = std::path::Path::new("/repo");
        let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);
        let prefixes = resolved(&[("a", "A")]);

        let base = walk_base(path("a/b/c"), root, &nodes, Some(&prefixes))
            .expect("an ancestor was created");
        assert_eq!(base.absolute, root.join("a/b"));
        assert_eq!(base.relative.as_str(), "a/b");
        assert_eq!(base.node, NODE_AB);
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn empty_path_no_layers_is_disjoint() {
        assert_eq!(classify_stage_path("", &[]), LayerRoute::Disjoint);
    }

    #[test]
    fn empty_path_with_layers_is_ancestor_of_all() {
        let layers = ["external/lib", "vendor/foo"];
        assert_eq!(
            classify_stage_path("", &layers),
            LayerRoute::AncestorOf {
                layer_indices: vec![0, 1],
            }
        );
    }

    #[test]
    fn exact_layer_match_is_inside_with_empty_remain() {
        let layers = ["external/lib"];
        assert_eq!(
            classify_stage_path("external/lib", &layers),
            LayerRoute::Inside {
                layer_index: 0,
                remain: RelativePath::new(),
            }
        );
    }

    #[test]
    fn path_inside_layer_is_inside_with_remain() {
        let layers = ["external/lib"];
        assert_eq!(
            classify_stage_path("external/lib/src/foo.rs", &layers),
            LayerRoute::Inside {
                layer_index: 0,
                remain: RelativePath::new_from_clean_parts("src/foo.rs", ""),
            }
        );
    }

    #[test]
    fn path_ancestor_of_one_layer_is_ancestor_of_that_layer() {
        let layers = ["external/lib", "src/main.rs"];
        assert_eq!(
            classify_stage_path("external", &layers),
            LayerRoute::AncestorOf {
                layer_indices: vec![0],
            }
        );
    }

    #[test]
    fn path_ancestor_of_multiple_layers_lists_them_all() {
        let layers = ["vendor/a", "vendor/b", "external/lib"];
        assert_eq!(
            classify_stage_path("vendor", &layers),
            LayerRoute::AncestorOf {
                layer_indices: vec![0, 1],
            }
        );
    }

    #[test]
    fn disjoint_path_with_layers_is_disjoint() {
        let layers = ["external/lib", "vendor/foo"];
        assert_eq!(
            classify_stage_path("src/main.rs", &layers),
            LayerRoute::Disjoint
        );
    }

    #[test]
    fn prefix_string_match_without_separator_is_disjoint_not_inside() {
        // "external" is a string prefix of "external_other" but not a path-prefix.
        // Confirms we check '/' boundary, not bare string prefix.
        let layers = ["external"];
        assert_eq!(
            classify_stage_path("external_other", &layers),
            LayerRoute::Disjoint
        );
    }
}
