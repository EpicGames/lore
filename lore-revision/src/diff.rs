// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::change;
use crate::change::NodeChange;
use crate::errors::InvalidArguments;
use crate::filter::FilterMode;
use crate::lore_debug;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::ChangeSink;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum DiffError {
    InvalidArguments,
}

/// Calculate the difference between two revisions, as the set of changes
/// that describe going from revision 'source' to revision 'target',
/// optionally filtered by a set of paths. Emits each change into `tx` as
/// the per-path tasks discover them; concurrent per-path tasks may
/// interleave their items on the channel.
///
/// Each per-path task streams its `state::diff` output directly into the
/// shared sender — there is no per-path-task buffering. Items arrive in
/// `state::diff`'s natural walk order (sorted by name-hash within each
/// subtree pair) and per-path blocks may interleave by completion order.
/// Callers that need a globally-sorted `Vec` drain via `collect_stream`
/// and apply `change::sort_by_path` themselves, or sort client-side after
/// consuming the stream.
pub async fn diff_revision_paths(
    repository: Arc<RepositoryContext>,
    state_source: Arc<State>,
    state_target: Arc<State>,
    paths: Option<Vec<RelativePath>>,
    tx: mpsc::Sender<Result<NodeChange, DiffError>>,
) -> Result<(), DiffError> {
    let mut tasks: JoinSet<Result<(), DiffError>> = JoinSet::new();
    let paths = paths.unwrap_or_else(|| vec![RelativePath::new()]);
    for path in paths.iter() {
        let repository = repository.clone();
        let state_source = state_source.clone();
        let state_target = state_target.clone();
        let path = if !path.is_empty() {
            Some(path.clone())
        } else {
            None
        };
        let task_tx = tx.clone();

        lore_spawn!(tasks, async move {
            // Stream straight from `state::diff`'s walker through a thin
            // per-task adapter into the shared sender — no per-path
            // buffering. `state::diff` emits `Result<NodeChange, StateError>`
            // on its inner channel; the adapter loop converts each item to
            // `Result<NodeChange, DiffError>` and forwards to the shared
            // `task_tx`. The inner channel is bounded so the engine
            // backpressures on a slow downstream consumer the same way
            // the outer channel does.
            let (state_tx, mut state_rx) =
                mpsc::channel::<Result<NodeChange, crate::state::StateError>>(256);
            let walker_repo = repository.clone();
            let walker = lore_spawn!(async move {
                let mut sink = ChangeSink::Channel(&state_tx);
                state::diff(
                    walker_repo.clone(),
                    state_source,
                    walker_repo,
                    state_target,
                    path,
                    None,
                    &mut sink,
                    FilterMode::View,
                )
                .await
            });
            while let Some(item) = state_rx.recv().await {
                let item = item.internal("calculating revision diff")?;
                task_tx
                    .send(Ok(item))
                    .await
                    .internal("revision diff receiver dropped")?;
            }
            // Surface any error from the walker task itself.
            match walker.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => Err(err).internal("calculating revision diff")?,
                Err(join_err) => {
                    return Err(DiffError::internal_with_context(
                        join_err,
                        "revision diff walker task failed",
                    ));
                }
            }
            Ok(())
        });
    }

    // Drop the parent sender clone so the receiver completes once all task
    // clones drop their senders.
    drop(tx);

    let mut final_error: Result<(), DiffError> = Ok(());
    let mut task_error: Result<(), DiffError> = Ok(());
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                final_error = Err(err);
            }
            Err(join_err) => {
                task_error = Err(DiffError::internal_with_context(
                    join_err,
                    "revision diff task failed",
                ));
            }
        }
    }
    final_error?;
    task_error?;

    Ok(())
}

pub async fn diff_filesystem_paths(
    repository: Arc<RepositoryContext>,
    state_from: Arc<State>,
    state_current: Arc<State>,
    paths: Option<Vec<RelativePath>>,
) -> Result<Vec<NodeChange>, DiffError> {
    let mut tasks: JoinSet<Result<Vec<NodeChange>, DiffError>> = JoinSet::new();
    let paths = paths.unwrap_or_else(|| vec![RelativePath::new()]);
    for path in paths.iter() {
        let repository = repository.clone();
        let state_from = state_from.clone();
        let state_current = state_current.clone();
        let path = path.clone();
        let exists = if !path.is_empty() {
            let mut exists_in_state = false;
            let mut exists_in_filesystem = false;

            let node_link = state_from
                .find_node_link(repository.clone(), path.as_str())
                .await
                .unwrap_or_default();
            if node_link.is_valid() {
                exists_in_state = true;
            } else {
                let absolute_path = path.to_absolute_path(repository.require_path()?);
                exists_in_filesystem = lore_io::IoDriver::global()
                    .metadata(absolute_path)
                    .await
                    .is_ok();
            }

            if !exists_in_state && !exists_in_filesystem {
                emit_path_ignore(path.as_str()).await;
                lore_debug!("Ignoring invalid path: {path}");
            }

            exists_in_state || exists_in_filesystem
        } else {
            true
        };

        if exists {
            lore_spawn!(tasks, {
                async move {
                    if !path.is_empty() {
                        lore_debug!(
                            "Calculating deltas against filesystem path: {}",
                            path.as_str()
                        );
                    } else {
                        lore_debug!("Calculating deltas against filesystem for full repository");
                    }

                    let (mut changes, _) = state::diff_filesystem(
                        repository.clone(),
                        state_from,
                        repository.clone(),
                        state_current,
                        if !path.is_empty() { Some(path) } else { None },
                        FilterMode::Full,
                        std::sync::Arc::new(Vec::new()),
                    )
                    .await
                    .internal("calculating filesystem diff")?;

                    lore_debug!("Found {} file system changes", changes.len());

                    change::sort_by_path(&mut changes);

                    Ok(changes)
                }
            });
        }
    }

    let mut changes = vec![];
    let mut final_error: Result<(), DiffError> = Ok(());
    let mut task_error: Result<(), DiffError> = Ok(());
    while let Some(result) = tasks.join_next().await {
        if let Ok(result) = result {
            match result {
                Ok(mut result) => {
                    changes.append(&mut result);
                }
                Err(err) => {
                    final_error = Err(err);
                }
            }
        } else {
            task_error = Err(DiffError::internal_with_context(
                result.unwrap_err(),
                "filesystem diff task failed",
            ));
        }
    }
    final_error?;
    task_error?;

    change::sort_by_path(&mut changes);

    Ok(changes)
}
