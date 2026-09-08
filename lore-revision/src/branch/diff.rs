// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;

use crate::branch;
use crate::branch::BranchError;
use crate::lore::execution_context;
use crate::repository::RepositoryContext;
use crate::state;
use crate::util::path::RelativePath;

pub async fn diff(
    repository: Arc<RepositoryContext>,
    source: String,
    target: String,
    path: String,
    auto_resolve: bool,
) -> Result<(), BranchError> {
    let source_branch = if source.is_empty() {
        let (_revision, branch) =
            crate::instance::load_current_anchor(&repository)
                .await
                .forward::<BranchError>("Failed to deserialize current revision anchor")?;
        branch.to_string()
    } else {
        source
    };
    let source_branch = branch::resolve(repository.clone(), source_branch.as_str()).await?;

    let target_branch = branch::resolve(repository.clone(), target.as_str()).await?;

    let path = if path.is_empty() {
        None
    } else {
        let path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
            .forward::<BranchError>("Invalid path")?;
        Some(path)
    };

    let mut source_latest = branch::load_latest(repository.clone(), source_branch.id)
        .await
        .unwrap_or_default();
    let mut target_latest = branch::load_latest(repository.clone(), target_branch.id)
        .await
        .unwrap_or_default();

    if !execution_context().globals().local()
        && let Ok(remote) = repository.remote().await
    {
        let source_task = {
            let remote = remote.clone();
            let source_branch = source_branch.clone();
            let repository_id = repository.id;
            lore_spawn!(async move {
                if !source_branch.local {
                    source_branch.latest
                } else {
                    branch::load_remote_latest(remote, repository_id, source_branch.id)
                        .await
                        .unwrap_or_default()
                }
            })
        };
        let target_task = {
            let remote = remote.clone();
            let target_branch = target_branch.clone();
            let repository_id = repository.id;
            lore_spawn!(async move {
                if !target_branch.local {
                    target_branch.latest
                } else {
                    branch::load_remote_latest(remote, repository_id, target_branch.id)
                        .await
                        .unwrap_or_default()
                }
            })
        };
        let (source_result, target_result) = tokio::join!(source_task, target_task);
        let source_remote_latest = source_result.internal("loading source remote latest")?;
        let target_remote_latest = target_result.internal("loading target remote latest")?;

        // If remote is > local, use remote
        if source_latest.is_zero() {
            source_latest = source_remote_latest;
        } else if let Ok(local_revision) =
            state::State::deserialize(repository.clone(), source_latest).await
        {
            if let Ok(remote_revision) =
                state::State::deserialize(repository.clone(), source_remote_latest).await
                && local_revision.revision_number() > 0
                && remote_revision.revision_number() > local_revision.revision_number()
            {
                source_latest = source_remote_latest;
            }
        } else {
            source_latest = source_remote_latest;
        }

        // If remote is > local, use remote
        if target_latest.is_zero() {
            target_latest = target_remote_latest;
        } else if let Ok(local_revision) =
            state::State::deserialize(repository.clone(), target_latest).await
        {
            if let Ok(remote_revision) =
                state::State::deserialize(repository.clone(), target_remote_latest).await
                && local_revision.revision_number() > 0
                && remote_revision.revision_number() > local_revision.revision_number()
            {
                target_latest = target_remote_latest;
            }
        } else {
            target_latest = target_remote_latest;
        }
    }

    if source_latest.is_zero() {
        return Err(BranchError::internal(
            "Unable to find latest revision of source branch",
        ));
    }
    if target_latest.is_zero() {
        return Err(BranchError::internal(
            "Unable to find latest revision of target branch",
        ));
    }

    let (source_metadata, target_metadata) = tokio::join!(
        branch::metadata(repository.clone(), source_branch.id),
        branch::metadata(repository.clone(), target_branch.id),
    );
    let source_name = source_metadata
        .map(|metadata| branch::name(&metadata).unwrap_or_default().to_string())
        .unwrap_or_default();
    let target_name = target_metadata
        .map(|metadata| branch::name(&metadata).unwrap_or_default().to_string())
        .unwrap_or_default();

    let diff = Box::pin(branch::diff3_collect(
        repository,
        source_branch.id,
        source_latest,
        target_branch.id,
        target_latest,
        path,
        false, /* Do not include identical changes */
        auto_resolve,
    ))
    .await?;

    branch::dispatch_diff_events(
        &diff,
        source_branch.id,
        source_name.as_str(),
        target_branch.id,
        target_name.as_str(),
    );

    Ok(())
}
