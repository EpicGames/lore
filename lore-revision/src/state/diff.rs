// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::task::JoinSet;

use crate::change;
use crate::change::NodeChangeState;
use crate::filter::FilterMode;
use crate::lore::Address;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_trace;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::repository::RepositoryContext;
use crate::state::ChangeSink;
use crate::state::OwnedChangeSink;
use crate::state::State;
use crate::state::StateChildrenNodes;
use crate::state::StateError;
use crate::state::StateNamedNode;
use crate::state::add_change;
use crate::state::named_node_sort;
use crate::util::path::RelativePath;

/// Decides whether a source subtree can be adopted whole during a merge.
///
/// Two things must hold. The view must exclude the whole subtree, so nothing in
/// it reaches the working tree. And the target's version of the directory must
/// be byte-identical to the base's, which means the branch changed nothing
/// inside it at any depth.
pub struct GraftOracle {
    repository: Arc<RepositoryContext>,
    state_target: Arc<State>,
    view: Arc<crate::filter::Filter>,
}

impl GraftOracle {
    pub fn new(
        repository: Arc<RepositoryContext>,
        state_target: Arc<State>,
        view: Arc<crate::filter::Filter>,
    ) -> Self {
        Self {
            repository,
            state_target,
            view,
        }
    }

    /// True when `path` in the target tree still matches `base_address` and
    /// holds no uncommitted work.
    async fn adoptable(&self, path: &RelativePath, base_address: Address) -> bool {
        // An out-of-view subtree never reaches the working tree, so merging it
        // is a tree operation only. An in-view subtree keeps the per-file
        // merge, because there the detail is needed on disk.
        //
        // Every descendant must be excluded, not just the directory node.
        if !self.view.excludes_subtree(path, FilterMode::View) {
            return false;
        }
        let Ok(link) = self
            .state_target
            .find_node_link(self.repository.clone(), path.as_str())
            .await
        else {
            return false;
        };
        if !link.is_valid_or_root() {
            return false;
        }
        // `find_node_link` resolves through links. A path in another
        // repository is merged through that link instead.
        if link.repository != self.repository.id {
            return false;
        }
        let Ok(node) = self
            .state_target
            .node(self.repository.clone(), link.node)
            .await
        else {
            return false;
        };
        if node.is_link() || node.is_file() {
            return false;
        }
        // Target's subtree is identical to base's.
        if node.address != base_address {
            return false;
        }
        // Do not adopt over uncommitted work.
        if node.is_staged() || node.is_dirty() {
            return false;
        }
        self.state_target
            .node_has_staged_children(self.repository.clone(), link.node)
            .await
            .is_ok_and(|has_staged| !has_staged)
    }
}

pub async fn diff_subtree(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    flags: u32,
    graft: Option<Arc<GraftOracle>>,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    if to.repository.filter.emit_excludes(&path, true, filter_mode) {
        lore_debug!("Excluded by filter: {}", path.as_str());
        return Ok(());
    }

    diff_subtree_node(
        from,
        to,
        DiffPaths {
            from: path.clone(),
            to: path,
        },
        flags,
        graft,
        sink,
        filter_mode,
    )
    .await
}

fn recurse_diff_subtree_node(
    from: NodeChangeState,
    to: NodeChangeState,
    paths: DiffPaths,
    flags: u32,
    graft: Option<Arc<GraftOracle>>,
    mut sink: OwnedChangeSink,
    filter_mode: FilterMode,
) -> Pin<Box<dyn Future<Output = Result<OwnedChangeSink, StateError>> + Send>> {
    Box::pin(async move {
        let mut local = sink.as_sink();
        diff_subtree_node(from, to, paths, flags, graft, &mut local, filter_mode).await?;
        Ok(sink)
    })
}

struct DiffPaths {
    from: RelativePath,
    to: RelativePath,
}

async fn diff_subtree_node(
    from: NodeChangeState,
    to: NodeChangeState,
    paths: DiffPaths,
    flags: u32,
    graft: Option<Arc<GraftOracle>>,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    // If path is a file then treat this as a call for the parent with only one child.
    let mut possibly_parent_paths = paths;
    let (from_nodes, to_nodes) =
        find_sorted_children(&mut possibly_parent_paths, &from, &to).await?;
    let paths = possibly_parent_paths;

    let mut subtasks = JoinSet::new();

    // Run the walk in a helper so any `?` early-out still hits the
    // drain below — otherwise the JoinSet drops with subtree-diff
    // tasks still running, leaking the Arc<RepositoryContext> clones.
    let work_result = diff_subtree_node_walk(
        &from,
        &to,
        &paths,
        flags,
        graft,
        sink,
        filter_mode,
        &from_nodes,
        &to_nodes,
        &mut subtasks,
    )
    .await;
    let drain_result = lore_drain_tasks!(subtasks, StateError::internal("Task failure"));
    work_result?;
    drain_result?;
    Ok(())
}

/// Walk paired/solo from/to children, spawning paired-node diffs into
/// `subtasks` and emitting solo changes inline.
///
/// Each (case-normalized) name is in one of three buckets:
/// 1. In `to_nodes` but not `from_nodes` → Added or Moved.
/// 2. In `from_nodes` but not `to_nodes` → Deleted or was Moved.
/// 3. In both → Modified, case-only rename, or unchanged.
#[allow(clippy::too_many_arguments)]
async fn diff_subtree_node_walk(
    from: &NodeChangeState,
    to: &NodeChangeState,
    paths: &DiffPaths,
    flags: u32,
    graft: Option<Arc<GraftOracle>>,
    sink: &mut ChangeSink<'_>,
    filter_mode: FilterMode,
    from_nodes: &StateChildrenNodes,
    to_nodes: &StateChildrenNodes,
    subtasks: &mut JoinSet<Result<OwnedChangeSink, StateError>>,
) -> Result<(), StateError> {
    let mut to_index = 0;
    for from_named_node in from_nodes.children.iter() {
        let Some(from_node_search) =
            get_filtered_node_and_path(from_nodes, from_named_node.node, &paths.from, filter_mode)
                .await?
        else {
            continue;
        };

        while to_index < to_nodes.children.len()
            && to_nodes.children[to_index].name < from_named_node.name
        {
            add_change_for_solo_to_node(
                DiffContext {
                    sink,
                    from_nodes,
                    to_nodes,
                    paths,
                    filter_mode,
                },
                from,
                to_index,
            )
            .await?;
            to_index += 1;
        }

        if to_index >= to_nodes.children.len()
            || to_nodes.children[to_index].name > from_named_node.name
        {
            add_change_for_solo_from_node(
                DiffContext {
                    sink,
                    from_nodes,
                    to_nodes,
                    paths,
                    filter_mode,
                },
                from_named_node,
                to,
                &from_node_search,
            )
            .await?;
        } else {
            let to_named_node = &to_nodes.children[to_index];
            to_index += 1;

            add_change_for_paired_nodes(
                subtasks,
                flags,
                graft.clone(),
                DiffContext {
                    sink,
                    from_nodes,
                    to_nodes,
                    paths,
                    filter_mode,
                },
                to_named_node,
                from_named_node.node,
                &from_node_search,
            )
            .await?;
        }
    }

    for to_index in to_index..to_nodes.children.len() {
        add_change_for_solo_to_node(
            DiffContext {
                sink,
                from_nodes,
                to_nodes,
                paths,
                filter_mode,
            },
            from,
            to_index,
        )
        .await?;
    }

    while let Some(task_result) = subtasks.join_next().await {
        let task_sink = task_result
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()?;
        sink.finalize_task_sink(task_sink);
    }

    Ok(())
}

struct DiffContext<'a, 'b> {
    sink: &'a mut ChangeSink<'b>,
    from_nodes: &'a StateChildrenNodes,
    to_nodes: &'a StateChildrenNodes,
    paths: &'a DiffPaths,
    filter_mode: FilterMode,
}

async fn add_change_for_solo_from_node(
    context: DiffContext<'_, '_>,
    from_named_node: &StateNamedNode,
    to: &NodeChangeState,
    from_node_search: &NodeSearchResult,
) -> Result<(), StateError> {
    let DiffContext {
        sink,
        from_nodes,
        to_nodes,
        filter_mode,
        ..
    } = context;
    let NodeSearchResult {
        node: from_node,
        path: from_path,
    } = from_node_search;
    // Before marking as deleted, check if this node was moved to a different location
    // in the staged state (same node ID, different name/parent)
    if from_named_node.node.is_valid_node_id()
        && let Some(check_node) = to_nodes
            .state
            .try_node(to_nodes.repository.clone(), from_named_node.node)
            .await
        && check_node.is_staged_move()
        && let Ok(from_node) = from_nodes
            .state
            .node(from_nodes.repository.clone(), from_named_node.node)
            .await
        && from_node.address.context == check_node.address.context
    {
        // This node was moved, not deleted - skip adding delete change
        // The move will be reported from the "to" side iteration
        lore_trace!(
            "Node {} moved (not deleted), skipping delete change",
            from_named_node.node
        );
    } else {
        lore_trace!("Node {} deleted", from_named_node.node);

        let from = NodeChangeState {
            repository: from_nodes.repository.clone(),
            state: from_nodes.state.clone(),
            node: from_named_node.node,
            flags: NodeFlags::from_bits_retain(from_node.flags),
            address: from_node.address,
        };

        add_change(
            from,
            to.invalid(),
            change::FileAction::Delete,
            from_path,
            None,
            sink,
            filter_mode,
        )
        .await?;
    }
    Ok(())
}

async fn add_change_for_solo_to_node(
    context: DiffContext<'_, '_>,
    from: &NodeChangeState,
    to_index: usize,
) -> Result<(), StateError> {
    let DiffContext {
        sink,
        from_nodes,
        to_nodes,
        paths,
        filter_mode,
    } = context;
    let to_named_node = &to_nodes.children[to_index];
    let Some(NodeSearchResult {
        node: to_node,
        path: subpath,
    }) = get_filtered_node_and_path(to_nodes, to_named_node.node, &paths.to, filter_mode).await?
    else {
        return Ok(());
    };

    // Determine the action and from_path for moved nodes
    let (file_action, from_path) = if to_node.is_staged_delete() {
        lore_trace!("Node {} deleted", to_named_node.node);
        (change::FileAction::Delete, None)
    } else if to_node.is_staged_move() {
        // Look up the original path from the from state
        let original_path = from_nodes
            .state
            .node_path(from_nodes.repository.clone(), to_named_node.node)
            .await
            .ok();
        lore_trace!(
            "Node {} moved from {:?} to {}",
            to_named_node.node,
            original_path,
            subpath
        );
        (
            change::FileAction::Move,
            original_path.map(|p| RelativePath::new_from_initial_path(&p).unwrap_or_default()),
        )
    } else {
        lore_trace!("Node {} added", to_named_node.node);
        (change::FileAction::Add, None)
    };

    let to = NodeChangeState {
        repository: to_nodes.repository.clone(),
        state: to_nodes.state.clone(),
        node: to_named_node.node,
        flags: NodeFlags::from_bits_retain(to_node.flags),
        address: to_node.address,
    };

    add_change(
        from.invalid(),
        to,
        file_action,
        &subpath,
        from_path.as_ref(),
        sink,
        filter_mode,
    )
    .await?;
    Ok(())
}

async fn add_change_for_paired_nodes(
    subtasks: &mut JoinSet<Result<OwnedChangeSink, StateError>>,
    flags: u32,
    graft: Option<Arc<GraftOracle>>,
    context: DiffContext<'_, '_>,
    to_named_node: &StateNamedNode,
    from_node_id: NodeID,
    from_node_search: &NodeSearchResult,
) -> Result<(), StateError> {
    let DiffContext {
        sink,
        from_nodes,
        to_nodes,
        paths,
        filter_mode,
    } = context;
    let NodeSearchResult {
        node: from_node,
        path: from_path,
    } = from_node_search;
    let to_block_index = NodeBlock::index(to_named_node.node);
    let to_node_index = Node::index(to_named_node.node);
    let to_block = to_nodes
        .state
        .block(to_nodes.repository.clone(), to_block_index)
        .await?;
    let to_node = to_block.node(to_node_index);

    // If hashes are equal we can skip this entire subtree - otherwise recurse and check
    let from_size = from_node.size;
    let to_size = to_node.size;
    let from_address = from_node.address;
    let to_address = to_node.address;
    let hash_equal = from_address == to_address;
    let from_mode = from_node.mode;
    let to_mode = to_node.mode;
    let mode_equal = from_mode == to_mode;
    let is_modify = !hash_equal || !mode_equal;
    let is_staged = to_node.is_staged();
    let is_staged_delete = to_node.is_staged_delete();
    let is_staged_merge = to_node.is_staged_merge();
    let is_dirty = to_node.is_dirty();
    let is_dirty_delete = to_node.is_dirty_delete();

    let to = NodeChangeState {
        repository: to_nodes.repository.clone(),
        state: to_nodes.state.clone(),
        node: to_named_node.node,
        flags: NodeFlags::from_bits_retain(to_node.flags),
        address: to_node.address,
    };

    let from = NodeChangeState {
        repository: from_nodes.repository.clone(),
        state: from_nodes.state.clone(),
        node: from_node_id,
        flags: NodeFlags::from_bits_retain(from_node.flags),
        address: from_node.address,
    };

    if is_staged_delete || is_dirty_delete {
        lore_trace!("Diff node {} deleted", &paths.to);

        add_change(
            from,
            to,
            change::FileAction::Delete,
            from_path,
            None,
            sink,
            filter_mode,
        )
        .await?;
    } else {
        let was_file = from_node.is_file();
        let is_file = to_node.is_file();

        let to_name = match to_nodes
            .state
            .node_name_ref(to_nodes.repository.clone(), to_named_node.node)
            .await
        {
            Ok(name) => name,
            Err(err) => {
                lore_warn!(
                    "Skipping node {} with invalid name: {err}",
                    to_named_node.node
                );
                return Ok(());
            }
        };
        let from_name = from_path.name();
        let is_rename = *from_name != *to_name;

        let subpath = paths.to.push_into_buf(&to_name).freeze();
        if is_rename {
            lore_trace!("Node is renamed from {from_name} -> {to_name}");
        }
        drop(to_name);

        let action = if is_rename {
            change::FileAction::Move
        } else {
            change::FileAction::Keep
        };

        if was_file && is_file {
            if is_modify || is_staged || is_staged_merge || is_dirty || is_rename {
                lore_trace!(
                    "Diff node {subpath} file modified {from_address} size {from_size} to {to_address} size {to_size}, mode {from_mode} to {to_mode} - {action:?}"
                );

                add_change(
                    from.clone(),
                    to.clone(),
                    action,
                    &subpath,
                    if is_rename { Some(from_path) } else { None },
                    sink,
                    filter_mode,
                )
                .await?;
            }
        } else if !was_file && !is_file {
            if !mode_equal || is_rename {
                lore_trace!(
                    "Diff node {subpath} directory mode change from {from_mode} to {to_mode}, {action:?}|modify"
                );
                add_change(
                    from.clone(),
                    to.clone(),
                    action,
                    &subpath,
                    if is_rename { Some(from_path) } else { None },
                    sink,
                    filter_mode,
                )
                .await?;
            }
            if !hash_equal || is_staged || is_staged_merge || is_dirty {
                if to_node.is_link() {
                    let linked_repository = Arc::new(
                        to.repository
                            .to_link_context(to_node.address.context.into())
                            .await,
                    );
                    let linked_state =
                        State::deserialize(linked_repository.clone(), to_node.address.hash)
                            .await
                            .internal("Link error")?;

                    let has_staged_children = linked_state
                        .node_has_staged_children(linked_repository.clone(), to_node.child)
                        .await?;

                    // If the link is staged and doesn't have staged children, it's a link update
                    let recurse_link = if is_staged { has_staged_children } else { true };

                    if !recurse_link {
                        lore_debug!("Diff node {subpath} has no linked changes");
                        add_change(
                            from.clone(),
                            to.clone(),
                            action,
                            &subpath,
                            None,
                            sink,
                            filter_mode,
                        )
                        .await?;
                    } else {
                        lore_debug!("Diff node {subpath} has linked changes, recurse diff");
                        let from_path = from_path.clone();
                        let task_sink = sink.task_sink();
                        lore_spawn!(subtasks, async move {
                            recurse_diff_subtree_node(
                                from,
                                to,
                                DiffPaths {
                                    from: from_path,
                                    to: subpath,
                                },
                                flags,
                                // A linked repository merges through its own
                                // link.
                                None,
                                task_sink,
                                filter_mode,
                            )
                            .await
                        });
                    }
                } else {
                    // Source changed this directory and the target branch did
                    // not, so nothing inside needs a three-way merge. Emit one
                    // change for the subtree instead of one per file.
                    let adopted = match graft.as_ref() {
                        Some(oracle) if !hash_equal => {
                            oracle.adoptable(&subpath, from_address).await
                        }
                        _ => false,
                    };

                    if adopted {
                        lore_trace!(
                            "Diff node {subpath} unchanged on target, grafting subtree {from_address} -> {to_address}"
                        );
                        add_change(
                            from.clone(),
                            to.clone(),
                            change::FileAction::Graft,
                            &subpath,
                            None,
                            sink,
                            filter_mode,
                        )
                        .await?;
                    } else {
                        lore_trace!(
                            "Diff node {subpath} directory hash change from {from_address} to {to_address}, recurse diff"
                        );
                        let from_path = from_path.clone();
                        let task_sink = sink.task_sink();
                        lore_spawn!(subtasks, async move {
                            recurse_diff_subtree_node(
                                from,
                                to,
                                DiffPaths {
                                    from: from_path,
                                    to: subpath,
                                },
                                flags,
                                graft,
                                task_sink,
                                filter_mode,
                            )
                            .await
                        });
                    }
                }
            }
        } else {
            lore_trace!(
                "Diff node {subpath} change from {} to {}, delete old and add new",
                if was_file { "file" } else { "directory" },
                if is_file { "file" } else { "directory" }
            );
            add_change(
                from.clone(),
                to.clone(),
                change::FileAction::Delete,
                from_path,
                None,
                sink,
                filter_mode,
            )
            .await?;
            add_change(
                from,
                to,
                change::FileAction::Add,
                &subpath,
                None,
                sink,
                filter_mode,
            )
            .await?;
        }
    }
    Ok(())
}

async fn find_sorted_children(
    directory_paths: &mut DiffPaths,
    from: &NodeChangeState,
    to: &NodeChangeState,
) -> Result<(StateChildrenNodes, StateChildrenNodes), StateError> {
    // If the given path and subtrees are files and not directories, which
    // can be the case when called from library interfaces like status with
    // an explicit path, we need to handle this here.
    let is_file_path = {
        let is_from_file = from
            .state
            .node(from.repository.clone(), from.node)
            .await
            .map(|node| node.is_file())
            .unwrap_or_default();
        let is_to_file = to
            .state
            .node(to.repository.clone(), to.node)
            .await
            .map(|node| node.is_file())
            .unwrap_or_default();
        is_from_file || is_to_file
    };

    Ok(if is_file_path {
        // Given path was a file node, treat it as enumerating the parent directory
        // and finding a single node for that file
        let mut from_nodes = StateChildrenNodes {
            repository: from.repository.clone(),
            state: from.state.clone(),
            children: vec![],
        };

        if from.node.is_valid_node_id()
            && let Ok(node) = from.state.node(from.repository.clone(), from.node).await
        {
            from_nodes.children.push(StateNamedNode {
                node: from.node,
                name: node.name_hash,
            });
        }

        let mut to_nodes = StateChildrenNodes {
            repository: to.repository.clone(),
            state: to.state.clone(),
            children: vec![],
        };

        if to.node.is_valid_node_id()
            && let Ok(node) = to.state.node(to.repository.clone(), to.node).await
        {
            to_nodes.children.push(StateNamedNode {
                node: to.node,
                name: node.name_hash,
            });
        }
        directory_paths.from.pop();
        directory_paths.to.pop();
        (from_nodes, to_nodes)
    } else {
        // Given path was a directory, enumerate all nodes
        let from_nodes = {
            let repository = from.repository.clone();
            let state = from.state.clone();
            let node = from.node;
            lore_spawn!(async move {
                let mut nodes = state
                    .collect_children_unsorted(
                        repository, node, false, /* No deleted nodes */
                        true,  /* Include links */
                    )
                    .await?;
                named_node_sort(&mut nodes.children);
                Ok::<_, StateError>(nodes)
            })
        };
        let to_nodes = {
            let repository = to.repository.clone();
            let state = to.state.clone();
            let node = to.node;
            lore_spawn!(async move {
                let mut nodes = state
                    .collect_children_unsorted(
                        repository, node, true, /* Include deleted nodes */
                        true, /* Include links */
                    )
                    .await?;
                named_node_sort(&mut nodes.children);
                Ok::<_, StateError>(nodes)
            })
        };

        let from_nodes = from_nodes.await;
        let to_nodes = to_nodes.await;

        (
            from_nodes
                .internal("Task failure")
                .map_err(StateError::from)??,
            to_nodes
                .internal("Task failure")
                .map_err(StateError::from)??,
        )
    })
}

pub struct NodeSearchResult {
    pub node: Node,
    pub path: RelativePath,
}

pub async fn get_node_and_path(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    path: &RelativePath,
) -> Result<Option<NodeSearchResult>, StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = nodes
        .state
        .block_with_nametable(nodes.repository.clone(), block_index)
        .await?;
    let node = block.node(node_index);
    let name = match block.node_name_ref(node_index) {
        Ok(name) => name,
        Err(err) => {
            lore_warn!("Skipping node {} with invalid name: {err}", node_id);
            return Ok(None);
        }
    };
    let path = path.push_into_buf(&name).freeze();

    Ok(Some(NodeSearchResult { node, path }))
}

pub async fn get_filtered_node_and_path(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    path: &RelativePath,
    filter_mode: FilterMode,
) -> Result<Option<NodeSearchResult>, StateError> {
    Ok(get_node_and_path(nodes, node_id, path)
        .await?
        .and_then(|result| {
            if nodes.repository.filter.emit_excludes(
                &result.path,
                result.node.is_directory(),
                filter_mode,
            ) {
                lore_trace!("Path excluded by filter: {}", path);
                None
            } else {
                Some(result)
            }
        }))
}
