// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;

use super::execution_context;
use crate::branch;
use crate::error::LoreResultExt;
use crate::errors::*;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::layer;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state::State;
use crate::util;

#[error_set]
pub enum CreateError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    BranchAlreadyExists,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    InvalidArguments,
    AddressNotFound,
    PayloadNotFound,
    AlreadyLinked,
    LayerNotFound,
    Disconnected,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotSupported,
    SlowDown,
    Maintenance,
    BranchAdvanced,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NotConnected,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for CreateError {
    fn translated(&self) -> LoreError {
        match self {
            Self::Disconnected(_) => LoreError::Connection,
            Self::SlowDown(_) => LoreError::SlowDown,
            Self::Oversized(_) => LoreError::Oversized,
            Self::FileNotFound(_) => LoreError::FileNotFound,
            Self::NotFound(_)
            | Self::BranchNotFound(_)
            | Self::RevisionNotFound(_)
            | Self::LayerNotFound(_) => LoreError::NotFound,
            Self::AddressNotFound(_) => LoreError::AddressNotFound,
            Self::PayloadNotFound(_) => LoreError::PayloadNotFound,
            Self::InvalidPath(_) | Self::InvalidArguments(_) => LoreError::InvalidArguments,
            Self::BranchAlreadyExists(_) => LoreError::AlreadyExists,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

pub async fn create(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: String,
    branch_id: Option<BranchId>,
    category: String,
    force: bool,
) -> Result<(), CreateError> {
    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<CreateError>("deserializing current and staged state")?;

    if state_current.revision().is_zero() {
        return Err(CreateError::internal(
            "Unable to create a branch without a previous revision, commit something first",
        ));
    }

    let branch_id = branch_id.unwrap_or_else(|| Context::from(uuid::Uuid::now_v7()));
    let user_id = execution_context().user_id().await;

    // Gather the parent branch stack
    let parent_metadata = branch::metadata(repository.clone(), current_branch)
        .await
        .forward::<CreateError>("loading parent branch metadata")?;
    let mut stack = branch::stack(&parent_metadata);
    stack.insert(
        0,
        BranchPoint {
            branch: current_branch,
            revision: state_current.revision(),
        },
    );

    // Create the branch
    match branch::create(
        repository.clone(),
        token,
        branch_id,
        branch.as_str(),
        category.as_str(),
        user_id.as_str(),
        util::time::timestamp(),
        stack.clone(),
        false,
        true, /* Create linked repositories branches */
    )
    .await
    {
        Ok(_) => (),
        Err(err) if err.is_branch_already_exists() => {
            if force {
                match branch::resolve(repository.clone(), branch.as_str()).await {
                    Ok(resolved) => {
                        let _ = branch::delete(repository.clone(), resolved.id)
                            .await
                            .debug_map_err(CreateError::internal(
                                "Failed to delete existing local branch",
                            ));
                        if let Ok(remote) = repository.remote().await {
                            let _ =
                                branch::delete_remote(remote.clone(), repository.id, resolved.id)
                                    .await;
                        }

                        // Retry the creation
                        branch::create(
                            repository.clone(),
                            token,
                            branch_id,
                            branch.as_str(),
                            category.as_str(),
                            user_id.as_str(),
                            util::time::timestamp(),
                            stack,
                            false,
                            true, /* Create linked repositories branches */
                        )
                        .await
                        .forward::<CreateError>("retrying branch creation after delete")?;
                    }
                    Err(_err) => {
                        return Err(CreateError::from(BranchAlreadyExists {
                            branch: branch.clone(),
                        }));
                    }
                }
            } else {
                // Still try to create branch in layers, in case branch does not exist in them, but ignore errors
                let _ = layer_branch_create(
                    repository.clone(),
                    token,
                    branch.clone(),
                    branch_id,
                    category,
                )
                .await;
                return Err(CreateError::from(BranchAlreadyExists { branch }));
            }
        }
        Err(err) => return Err(CreateError::internal_with_context(err, "creating branch")),
    }

    layer_branch_create(
        repository.clone(),
        token,
        branch.clone(),
        branch_id,
        category,
    )
    .await?;

    if let Some(state_staged) = state_staged {
        crate::instance::store_staged_anchor(&repository, state_staged.revision())
            .await
            .forward::<CreateError>("storing staged anchor")?;
    }

    crate::instance::store_current_anchor_branch(&repository, branch_id)
        .await
        .forward::<CreateError>("storing current anchor branch")?;
    crate::instance::store_current_anchor(&repository, state_current.revision())
        .await
        .forward::<CreateError>("storing current anchor")?;

    Ok(())
}

async fn layer_branch_create(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: String,
    branch_id: BranchId,
    category: String,
) -> Result<(), CreateError> {
    let layers = layer::list(repository.clone())
        .await
        .forward::<CreateError>("listing layers")?;

    lore_debug!("Creating branch {branch} for layers {layers:?}");

    for layer in layers {
        let layer_repository = Arc::new(repository.to_layer_context(layer.repository).await);
        let user_id = execution_context().user_id().await;

        let current_revision = State::deserialize(layer_repository.clone(), layer.current)
            .await
            .forward::<CreateError>("deserializing layer current state")?;
        let current_branch = current_revision.branch(layer_repository.clone()).await;
        if current_branch == branch_id {
            lore_debug!(
                "Layer for repository {} already on branch, not creating",
                layer.repository
            );
            continue;
        }

        let parent_metadata = branch::metadata(repository.clone(), current_branch)
            .await
            .forward::<CreateError>("loading layer parent branch metadata")?;
        let mut stack = branch::stack(&parent_metadata);
        stack.insert(
            0,
            BranchPoint {
                branch: current_branch,
                revision: current_revision.revision(),
            },
        );

        match branch::create(
            layer_repository,
            token,
            branch_id,
            branch.as_str(),
            category.as_str(),
            user_id.as_str(),
            util::time::timestamp(),
            stack,
            false,
            true, /* Create linked repositories branches */
        )
        .await
        {
            Ok(_id) => {}
            Err(err) if err.is_branch_already_exists() => {
                // TODO(mjansson): Ok if layer branch already exist, but switch to the branch
                return Err(CreateError::internal(
                    "Failed to create branch in layer, branch already exist",
                ));
            }
            Err(err) => {
                return Err(CreateError::internal_with_context(
                    err,
                    "creating branch in layer",
                ));
            }
        }
    }

    Ok(())
}
