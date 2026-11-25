// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use crate::filter::FilterMode;
use crate::hash;
use crate::lock;
use crate::lock::LockStore;
use crate::lore::BranchId;
use crate::node::Node;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;

/// The most resources one lock, unlock or status request names when no limit is given.
pub const LOCK_BATCH_SIZE: usize = 100;

/// How many resources belong in one lock, unlock or status request to the server `environment`
/// describes.
///
/// The limit the server advertises, or [`LOCK_BATCH_SIZE`] when it advertises none. Zero means no
/// advertised limit.
pub fn lock_batch_size(environment: &lore_transport::EnvironmentConfig) -> usize {
    environment
        .max_lock_batch()
        .filter(|size| *size > 0)
        .unwrap_or(LOCK_BATCH_SIZE)
}

/// The most resources one lock, unlock or status request may name against `store`.
///
/// The store's own limit, or [`LOCK_BATCH_SIZE`] when it names none.
pub fn store_lock_batch_size(store: &dyn LockStore) -> usize {
    store.max_lock_batch().unwrap_or(LOCK_BATCH_SIZE)
}

/// The tree a lock command reads its paths against, the staged one or the current one where
/// nothing is staged, and the branch the working tree is on.
pub async fn lock_tree(
    repository: &Arc<RepositoryContext>,
) -> Result<(Arc<State>, BranchId), StateError> {
    let (current_revision, branch) =
        crate::instance::load_current_anchor(repository)
            .await
            .forward::<StateError>("Failed to deserialize current revision anchor")?;
    let staged_revision = crate::instance::load_staged_revision(repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);
    Ok((
        State::deserialize_staged(repository.clone(), staged_revision, current_revision).await?,
        branch,
    ))
}

/// The node `tree` holds at `path`, which a lock command names, or `None` where the filter leaves
/// the path out, emitting a `LoreEvent::FilterExclude` there. Unless `force` puts the command past
/// the filter, the path is asked about as the kind the tree holds there, and is past the ignore slot
/// where the tree tracks it ([`State::tracks`]).
///
/// The tree is read again only where the filter would leave the path out, a cold path, which is
/// boxed.
pub async fn lock_path_node(
    repository: &Arc<RepositoryContext>,
    tree: &Arc<State>,
    path: &RelativePath,
    force: bool,
) -> Result<Option<Result<Node, StateError>>, StateError> {
    let node = tree.find_node(repository.clone(), path.as_str()).await;
    let filter = &repository.filter;
    let is_directory = Node::asked_as_directory(node.as_ref().ok());
    if force || !filter.excludes_tree(path, is_directory, FilterMode::Full) {
        return Ok(Some(node));
    }
    let tracked = node.is_ok()
        && Box::pin(tree.relative_path_tracked_as(repository.clone(), ROOT_NODE, path.as_str()))
            .await?
            .is_some();
    if filter.emit_excludes(path, is_directory, FilterMode::TRACKED.at(tracked)) {
        return Ok(None);
    }
    Ok(Some(node))
}

pub fn assemble_resource_for_path(path: &str, branch: BranchId) -> lock::LockResource {
    let hash = hash::hash_slice(path.as_bytes());
    let description = path.to_string();
    lock::LockResource {
        branch,
        hash,
        description,
    }
}

/// Folds per-batch outcomes into the collected items, the count of successful
/// batches, and the error of one failing batch, which carries the reason the
/// remote refused it. Batches complete out of order, so which failing batch the
/// error comes from is unspecified. `capacity` sizes the item vector up front
/// and is an upper bound on what the batches return.
pub fn fold_batch_results<T, E>(
    batch_results: Vec<Result<Vec<T>, E>>,
    capacity: usize,
) -> (Vec<T>, usize, Option<E>) {
    let mut items = Vec::with_capacity(capacity);
    let mut num_success = 0;
    let mut first_error = None;
    for batch_result in batch_results {
        match batch_result {
            Ok(mut batch_items) => {
                items.append(&mut batch_items);
                num_success += 1;
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    (items, num_success, first_error)
}
