// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::task::JoinSet;

use super::DEPENDENCIES_KEY;
use super::DependencyError;
use super::LoreDependencyResolveBeginEventData;
use super::LoreDependencyResolveEndEventData;
use super::LoreDependencyResolveItemEventData;
use super::add::find_own_node;
use super::load_dependency_data;
use crate::errors::InvalidArguments;
use crate::errors::NotSupported;
use crate::event;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore_warn;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util::path::RelativePath;

/// Compute the transitive closure of dependency relationships starting
/// from `root_nodes`, following edges stored under `key` (either
/// [`DEPENDENCIES_KEY`](super::DEPENDENCIES_KEY) or
/// [`DEPENDENTS_KEY`](super::DEPENDENTS_KEY)).
///
/// Uses concurrent BFS with spawned producer tasks that load dependency
/// data in parallel. A consumer loop collects discovered nodes, deduplicates
/// via a visited set, and spawns new producers for unvisited nodes.
///
/// If `tags` is non-empty, only edges whose entries contain at least one
/// of the specified tags are followed. A `depth_limit` of `0` means
/// unlimited depth.
///
/// When `emit_events` is `true`, producers emit
/// [`LoreEvent::DependencyResolveItem`](crate::event::LoreEvent::DependencyResolveItem)
/// events as dependencies are discovered, with the actual source and target
/// paths resolved from the graph traversal context and the matching tags
/// from each edge.
///
/// Returns the complete set of reachable `NodeID`s (excluding the root
/// nodes themselves unless they appear as transitive targets).
pub async fn transitive_closure(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    root_nodes: &[NodeID],
    key: &str,
    tags: &[&str],
    depth_limit: u32,
    emit_events: bool,
) -> Result<HashSet<NodeID>, DependencyError> {
    let key: Arc<str> = Arc::from(key);
    let tags: Arc<[Box<str>]> = tags.iter().map(|&t| Box::from(t)).collect();

    let mut visited = HashSet::new();
    let mut tasks: JoinSet<Result<Vec<(NodeID, u32)>, DependencyError>> = JoinSet::new();

    for &node in root_nodes {
        let repo = repository.clone();
        let st = state.clone();
        let k = key.clone();
        let t = tags.clone();
        lore_spawn!(tasks, async move {
            produce(repo, st, node, 0, &k, &t, emit_events).await
        });
    }

    let mut failure: Option<DependencyError> = None;
    while let Some(result) = tasks.join_next().await {
        match result.internal("dependency traversal task join") {
            Ok(Ok(children)) if failure.is_none() => {
                for (child_node, child_depth) in children {
                    if visited.insert(child_node) && (depth_limit == 0 || child_depth < depth_limit)
                    {
                        let repo = repository.clone();
                        let st = state.clone();
                        let k = key.clone();
                        let t = tags.clone();
                        lore_spawn!(tasks, async move {
                            produce(repo, st, child_node, child_depth, &k, &t, emit_events).await
                        });
                    }
                }
            }
            Ok(Err(err)) => {
                failure = failure.or(Some(err));
            }
            Err(err) => {
                failure = failure.or(Some(err.into()));
            }
            _ => {}
        }
    }

    match failure {
        Some(err) => Err(err),
        None => Ok(visited),
    }
}

async fn produce(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    current: NodeID,
    depth: u32,
    key: &str,
    tags: &[Box<str>],
    emit_events: bool,
) -> Result<Vec<(NodeID, u32)>, DependencyError> {
    let data = load_dependency_data(repository.clone(), &state, current, key).await?;
    let source_path = if emit_events {
        state.node_path(repository.clone(), current).await.ok()
    } else {
        None
    };
    let mut children = Vec::new();

    for entry in data.iter() {
        if !entry.matches_tags(tags) {
            continue;
        }

        if let Some(source_path) = &source_path
            && let Ok(target_path) = state.node_path(repository.clone(), entry.node).await
        {
            event::LoreEvent::DependencyResolveItem(LoreDependencyResolveItemEventData {
                source: LoreString::from(source_path.as_str()),
                target: LoreString::from(target_path.as_str()),
                tags: LoreArray::from_vec(
                    entry
                        .tags
                        .iter()
                        .map(|t| LoreString::from(t.as_ref()))
                        .collect(),
                ),
            })
            .send();
        }

        children.push((entry.node, depth + 1));
    }

    Ok(children)
}

/// Check whether adding edges from `source` to each node in `targets`
/// would create a dependency cycle.
///
/// For each target, computes the transitive closure of forward
/// dependencies starting from that target. If `source` is reachable
/// from any target, a cycle would be formed.
///
/// Returns `Ok(())` if no cycle is detected, or
/// `Err(DependencyError::InvalidArguments)` with a description of the cycle.
pub async fn check_cycle(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    source: NodeID,
    targets: &[NodeID],
    key: &str,
) -> Result<(), DependencyError> {
    for &target in targets {
        if target == source {
            return Err(InvalidArguments {
                reason: format!("direct self-dependency on node {source}"),
            }
            .into());
        }

        let reachable = transitive_closure(
            repository.clone(),
            state.clone(),
            &[target],
            key,
            &[],
            0,
            false,
        )
        .await?;

        if reachable.contains(&source) {
            return Err(InvalidArguments {
                reason: format!("node {source} -> {target} -> ... -> {source}"),
            }
            .into());
        }
    }

    Ok(())
}

/// The nodes of the root files `root_paths` in `state`, each with its repository path. A root
/// file inside a link is skipped with a warning, and `NotSupported` is returned when every root
/// file is, since file dependencies inside a link are not supported. Emits
/// `DependencyResolveBegin` with the number of root files kept.
pub(crate) async fn resolve_root_nodes(
    repository: &Arc<RepositoryContext>,
    state: &State,
    root_paths: &[String],
) -> Result<Vec<(NodeID, RelativePath)>, DependencyError> {
    let mut roots = Vec::with_capacity(root_paths.len());
    let mut skipped = false;
    for root_path in root_paths {
        let path = RelativePath::new_from_initial_path(root_path)
            .forward_with::<DependencyError, _>(|| format!("Invalid path: {root_path}"))?;
        if let Some(node) = find_own_node(repository, state, path.as_str()).await? {
            roots.push((node, path));
        } else {
            lore_warn!(
                "Skipping root file {path}: file dependencies inside a link are not supported"
            );
            skipped = true;
        }
    }
    if roots.is_empty() && skipped {
        return Err(NotSupported {
            operation: "file dependencies on root files that are all inside links".into(),
        }
        .into());
    }

    event::LoreEvent::DependencyResolveBegin(LoreDependencyResolveBeginEventData {
        root_count: roots.len() as u64,
    })
    .send();
    Ok(roots)
}

/// Resolve the complete set of files to include based on dependency metadata.
///
/// Starting from `root_paths`, resolves each to a `NodeID`, loads its forward
/// dependencies (filtered by `tags`), and optionally follows transitive
/// dependencies up to `depth_limit` levels deep. Root files are resolved as
/// [`resolve_root_nodes`] describes.
///
/// Returns the union of all root `NodeID`s and their resolved dependency `NodeID`s.
/// Emits `DependencyResolveBegin`, `DependencyResolveItem`, and `DependencyResolveEnd`
/// events during resolution.
pub async fn resolve_dependency_file_set(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    root_paths: &[String],
    tags: &[&str],
    recursive: bool,
    depth_limit: u32,
) -> Result<HashSet<NodeID>, DependencyError> {
    let root_nodes: Vec<NodeID> = resolve_root_nodes(&repository, &state, root_paths)
        .await?
        .into_iter()
        .map(|(node, _)| node)
        .collect();
    let mut inclusion_set: HashSet<NodeID> = root_nodes.iter().copied().collect();

    // Resolve dependencies — events are emitted from within the transitive
    // closure producers (recursive) or inline below (direct only)
    if recursive {
        let deps = transitive_closure(
            repository.clone(),
            state.clone(),
            &root_nodes,
            DEPENDENCIES_KEY,
            tags,
            depth_limit,
            true,
        )
        .await?;
        inclusion_set.extend(&deps);
    } else {
        // Direct dependencies only — load deps for each root
        for &root_node in &root_nodes {
            let data =
                load_dependency_data(repository.clone(), &state, root_node, DEPENDENCIES_KEY)
                    .await?;
            for entry in data.iter() {
                if !entry.matches_tags(tags) {
                    continue;
                }
                if inclusion_set.insert(entry.node)
                    && let (Ok(source_path), Ok(target_path)) = (
                        state.node_path(repository.clone(), root_node).await,
                        state.node_path(repository.clone(), entry.node).await,
                    )
                {
                    event::LoreEvent::DependencyResolveItem(LoreDependencyResolveItemEventData {
                        source: LoreString::from(source_path.as_str()),
                        target: LoreString::from(target_path.as_str()),
                        tags: LoreArray::from_vec(
                            entry
                                .tags
                                .iter()
                                .map(|t| LoreString::from(t.as_ref()))
                                .collect(),
                        ),
                    })
                    .send();
                }
            }
        }
    }

    event::LoreEvent::DependencyResolveEnd(LoreDependencyResolveEndEventData {
        resolved_count: inclusion_set.len() as u64,
    })
    .send();

    Ok(inclusion_set)
}
