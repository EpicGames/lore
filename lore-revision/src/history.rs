// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;

use crate::errors::*;
use crate::find;
use crate::lore::Address;
use crate::lore::Hash;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::store::StoreMatch;
use crate::store::query_one;

#[error_set]
pub enum HistoryError {
    AddressNotFound,
    Disconnected,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    RevisionNotFound,
    SlowDown,
    WriteRequired,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
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
    FileNotFound,
    MissingIdentity,
}

impl crate::event::EventError for HistoryError {}

pub async fn find_branch_point(
    repository: Arc<RepositoryContext>,
    left_latest: Hash,
    right_latest: Hash,
) -> Result<(Hash, Vec<Hash>, Vec<Hash>), HistoryError> {
    // Find the common ancestor of left branch and right branch, in case it has diverged
    // TODO(mjansson): Accelerate by storing last synced hash for each remote and branch
    let mut local_left = true;
    let mut local_right = true;
    let mut current_left = left_latest;
    let mut current_right = right_latest;
    let mut left_history = vec![];
    let mut right_history = vec![];
    while !current_left.is_zero() || !current_right.is_zero() {
        let mut left_fetch_count = 0;
        let mut right_fetch_count = 0;

        if local_left {
            for _fetch_count in 0..10 {
                if current_left.is_zero() {
                    break;
                }
                left_history.push(current_left);
                left_fetch_count += 1;

                if let Ok(matched) = query_one(
                    &repository.immutable_store(),
                    repository.id,
                    Address::zero_context_hash(current_left),
                )
                .await
                    && matched.match_made != StoreMatch::MatchNone
                {
                    let hash = current_left;
                    let state = State::deserialize(repository.clone(), current_left)
                        .await
                        .forward_with::<HistoryError, _>(|| {
                        format!("failed to deserialize state {hash}")
                    })?;
                    // TODO(mjansson): Support finding through merged and other parent
                    current_left = state.parent_self();
                } else {
                    local_left = false;
                    break;
                }
            }
        }
        if local_right {
            for _fetch_count in 0..10 {
                if current_right.is_zero() {
                    break;
                }
                right_history.push(current_right);
                right_fetch_count += 1;

                if let Ok(matched) = query_one(
                    &repository.immutable_store(),
                    repository.id,
                    Address::zero_context_hash(current_right),
                )
                .await
                    && matched.match_made != StoreMatch::MatchNone
                {
                    let hash = current_right;
                    let state = State::deserialize(repository.clone(), current_right)
                        .await
                        .forward_with::<HistoryError, _>(|| {
                            format!("failed to deserialize state {hash}")
                        })?;
                    // TODO(mjansson): Support finding through merged and other parent
                    current_right = state.parent_self();
                } else {
                    local_right = false;
                    break;
                }
            }
        }

        if !local_left || !local_right {
            let revision = if let Ok(remote) = repository.remote().await {
                remote.revision(repository.id).await.ok()
            } else {
                None
            };
            if let Some(revision) = revision.clone() {
                // TODO(mjansson): Support finding through merged and other parent
                let revision_remote = revision.clone();
                let left_response = lore_spawn!(async move {
                    if !local_left {
                        Some(revision_remote.revision_list(current_left.into()).await)
                    } else {
                        None
                    }
                });
                let revision_remote = revision.clone();
                let right_response = lore_spawn!(async move {
                    if !local_right {
                        Some(revision_remote.revision_list(current_right.into()).await)
                    } else {
                        None
                    }
                });

                let left_response = left_response.await.unwrap_or_default();
                let right_response = right_response.await.unwrap_or_default();

                // Only reset the side that needed a remote fetch. The other side may
                // still be walking locally — zeroing its cursor unconditionally would
                // halt the walk early and falsely report divergence even though a
                // common ancestor is still reachable.
                if !local_left {
                    current_left = Hash::default();
                    if let Some(Ok(left_response)) = left_response {
                        find::cache_revision_list_states(repository.clone(), &left_response.items)
                            .await;
                        left_fetch_count += left_response.items.len();
                        left_history.extend(left_response.items.iter().map(|item| item.signature));
                        current_left = left_response.next_revision;
                    }
                }

                if !local_right {
                    current_right = Hash::default();
                    if let Some(Ok(right_response)) = right_response {
                        find::cache_revision_list_states(repository.clone(), &right_response.items)
                            .await;
                        right_fetch_count += right_response.items.len();
                        right_history
                            .extend(right_response.items.iter().map(|item| item.signature));
                        current_right = right_response.next_revision;
                    }
                }
            }
        }

        // Find intersection, the common ancestor
        for iright in (right_history.len() - right_fetch_count)..right_history.len() {
            if let Some(left_iter) = left_history
                .iter()
                .enumerate()
                .find(|iter| *iter.1 == right_history[iright])
            {
                let branch_point = right_history[iright];
                right_history.truncate(iright);
                left_history.truncate(left_iter.0);
                return Ok((branch_point, left_history, right_history));
            }
        }

        for ileft in (left_history.len() - left_fetch_count)..left_history.len() {
            // No need to test against the newly fetched right hashes, already done above
            if let Some(right_iter) = right_history[..(right_history.len() - right_fetch_count)]
                .iter()
                .enumerate()
                .find(|iter| *iter.1 == left_history[ileft])
            {
                let branch_point = left_history[ileft];
                right_history.truncate(right_iter.0);
                left_history.truncate(ileft);
                return Ok((branch_point, left_history, right_history));
            }
        }

        if current_left.is_zero() && current_right.is_zero() {
            return Ok((Hash::default(), left_history, right_history));
        }
    }

    Err(HistoryError::internal("failed to find a branch point"))
}
