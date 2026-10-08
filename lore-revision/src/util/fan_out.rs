// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::future::Future;

use futures::FutureExt;
use lore_base::lore_spawn;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use super::path::DepthPath;
use super::path::shared_component_depth;
use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::node::NodeID;

/// The node of each shared ancestor already created, borrowed from the list they
/// are created from.
#[lore_macro::test_pub]
pub(crate) type AncestorNodes<'a> = HashMap<&'a str, NodeID>;

/// The deepest strict ancestor of `path` that has a node, and that node.
///
/// Starts at the parent, never at `path` itself: the caller is about to create or
/// walk `path`, and starting on top of it would skip it.
#[lore_macro::test_pub]
pub(crate) fn longest_ancestor<'a>(
    path: &'a str,
    nodes: &AncestorNodes<'_>,
) -> Option<(&'a str, NodeID)> {
    let mut end = path.rfind('/')?;
    loop {
        let candidate = &path[..end];
        if let Some(node) = nodes.get(candidate) {
            return Some((candidate, *node));
        }
        end = candidate.rfind('/')?;
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
/// The result is prefix-closed and holds one case variation of each entry when
/// the targets do, as [`RelativePath::dedup_to_supersets`] leaves them, so a
/// depth is a set of distinct nodes whose parents the depth above holds.
///
/// [`RelativePath::dedup_to_supersets`]: super::path::RelativePath::dedup_to_supersets
#[lore_macro::test_pub]
pub(crate) fn shared_ancestors<P: AsRef<str>>(targets: &[P]) -> Vec<DepthPath> {
    let mut shared: Vec<DepthPath> = Vec::new();
    let mut preceding = 0;
    for (index, target) in targets.iter().enumerate() {
        let target = target.as_ref();
        let following = targets
            .get(index + 1)
            .map_or(0, |next| shared_component_depth(target, next.as_ref()));
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

/// Create the node of every directory in `ancestors`, a depth level at a time, and
/// answer the node of each one created.
///
/// `ancestors` is a [`shared_ancestors`] list, so a level's nodes are the next
/// level's parents: each level is drained before the next starts, and `create` is
/// handed the nodes of the levels above to start from. At most
/// [`MAX_CONCURRENT_TREE_TASKS`] creations of a level run at once. A creation
/// answers the node it created or found, or `None` where it has none.
///
/// A creation in flight is allocating nodes and is drained even where an earlier
/// one failed, rather than cancelled part way through. No creation starts after
/// the first failure, which is the one returned.
#[lore_macro::test_pub]
pub(crate) async fn create_shared_ancestors<'a, E, Fut>(
    ancestors: &'a [DepthPath],
    mut create: impl FnMut(&'a str, &AncestorNodes<'a>) -> Fut,
    join_failure: impl Fn(JoinError) -> E,
) -> Result<AncestorNodes<'a>, E>
where
    E: Send + 'static,
    Fut: Future<Output = Result<Option<NodeID>, E>> + Send + 'static,
{
    let mut nodes = AncestorNodes::with_capacity(ancestors.len());
    let mut failure: Option<E> = None;

    for level in ancestors.chunk_by(|left, right| left.depth() == right.depth()) {
        let mut level_tasks = JoinSet::new();
        for (index, ancestor) in level.iter().enumerate() {
            if failure.is_some() {
                break;
            }
            let created = create(ancestor.path(), &nodes);
            lore_spawn!(level_tasks, created.map(move |created| (index, created)));
            join_below(
                &mut level_tasks,
                MAX_CONCURRENT_TREE_TASKS,
                &mut failure,
                &join_failure,
                |(index, created)| record_created(level, index, created, &mut nodes),
            )
            .await;
        }
        join_below(
            &mut level_tasks,
            1,
            &mut failure,
            &join_failure,
            |(index, created)| record_created(level, index, created, &mut nodes),
        )
        .await;
        if failure.is_some() {
            break;
        }
    }

    match failure {
        Some(err) => Err(err),
        None => Ok(nodes),
    }
}

/// Record the node the creation of `level[index]` answered, passing on its failure.
fn record_created<'a, E>(
    level: &'a [DepthPath],
    index: usize,
    created: Result<Option<NodeID>, E>,
    nodes: &mut AncestorNodes<'a>,
) -> Result<(), E> {
    if let Some(node) = created? {
        nodes.insert(level[index].path(), node);
    }
    Ok(())
}

/// Join the finished tasks of `tasks`, waiting on the rest while `limit` or more remain, and
/// hand each output to `collect`. A `limit` of 1 joins every task.
///
/// The first failure is kept in `failure` rather than returned, since a task in flight is left
/// to finish: a task that panicked or was cancelled, through `join_failure`, or an output
/// `collect` refuses.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
pub(crate) fn join_below<'a, T: 'static, E>(
    tasks: &'a mut JoinSet<T>,
    limit: usize,
    failure: &'a mut Option<E>,
    join_failure: impl Fn(JoinError) -> E + 'a,
    mut collect: impl FnMut(T) -> Result<(), E> + 'a,
) -> impl Future<Output = ()> + 'a {
    async move {
        loop {
            let joined = match tasks.try_join_next() {
                Some(joined) => joined,
                None if tasks.len() >= limit => match tasks.join_next().await {
                    Some(joined) => joined,
                    None => break,
                },
                None => break,
            };
            if let Err(err) = joined.map_err(&join_failure).and_then(&mut collect) {
                failure.get_or_insert(err);
            }
        }
    }
}
