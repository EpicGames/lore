// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::change;
use crate::change::NodeChange;
use crate::errors::InvalidArguments;
use crate::errors::SlowDown;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::FilesystemDiffTree;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::lore_debug;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum DiffError {
    InvalidArguments,
    SlowDown,
}

/// How many changes one path's walk may run ahead of the task forwarding them on.
///
/// Shallower than a walk read directly would take, on two counts: there is one of these per
/// requested path, and every change one holds is held a second time in the shared channel it is
/// forwarded to. What depth is for is keeping a walk's parallelism busy, and a walk that is
/// already running behind a second channel has that.
const PATH_LOOKAHEAD: NonZeroUsize = NonZeroUsize::new(256).expect("a nonzero literal");

/// Calculate the difference between two revisions, as the set of changes
/// that describe going from revision 'source' to revision 'target',
/// optionally filtered by a set of paths. Emits each change into `tx` as
/// the per-path tasks discover them; concurrent per-path tasks may
/// interleave their items on the channel.
///
/// Each per-path task streams its `state::diff` output directly into the
/// shared sender, buffering no more than `PATH_LOOKAHEAD`, so a slow
/// consumer holds its walk back rather than being outrun by it. Items arrive in
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
            let walker_repo = repository.clone();
            let mut walk =
                state::ChangeStream::spawn_with_lookahead(PATH_LOOKAHEAD, async move |changes| {
                    state::diff(
                        walker_repo.clone(),
                        state_source,
                        walker_repo,
                        state_target,
                        path,
                        None,
                        &changes,
                        FilterMode::View,
                    )
                    .await
                });
            while let Some(change) = walk.next().await {
                task_tx
                    .send(Ok(change))
                    .await
                    .internal("revision diff receiver dropped")?;
            }
            walk.finish()
                .await
                .forward_any::<DiffError>("calculating revision diff")?;
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
    let filesystem = repository.file_system();
    with_operation(filesystem, async |operation| {
        diff_filesystem_paths_in(operation, repository, state_from, state_current, paths).await
    })
    .await
}

/// The changes every path in `paths` shows against the filesystem, walked concurrently
/// through one operation.
async fn diff_filesystem_paths_in(
    operation: Arc<InstanceOperationImpl>,
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
        let operation = operation.clone();
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
                let repository_path = path.clone();
                exists_in_filesystem = operation
                    .file_info(&repository_path)
                    .await
                    .is_ok_and(|info| info.exists());
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

                    let mut changes = state::diff_filesystem(
                        &operation,
                        FilesystemDiffTree {
                            repository: repository.clone(),
                            state: state_from,
                        },
                        FilesystemDiffTree {
                            repository: repository.clone(),
                            state: state_current,
                        },
                        if !path.is_empty() { Some(path) } else { None },
                        FilterMode::Full,
                        FilesystemDiffIntent::Report,
                        std::sync::Arc::new(Vec::new()),
                    )
                    .await
                    .forward_any::<DiffError>("calculating filesystem diff")?
                    .collect()
                    .await
                    .forward_any::<DiffError>("calculating filesystem diff")?;

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
