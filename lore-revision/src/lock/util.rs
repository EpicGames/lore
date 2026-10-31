// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use crate::hash;
use crate::lock;
use crate::lore::BranchId;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;

pub const LOCK_BATCH_SIZE: usize = 100;

/// The tree a lock command reads its paths against, the staged one or the current one where
/// nothing is staged, and the branch the working copy is on.
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
        State::deserialize(repository.clone(), staged_revision).await?,
        branch,
    ))
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
