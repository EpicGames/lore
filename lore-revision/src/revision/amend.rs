// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use crate::commit::LoreRevisionCommitRevisionEventData;
use crate::commit::store_branch_latest_and_make_current;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::metadata;
use crate::metadata::Metadata;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state::State;

#[error_set]
pub enum AmendRevisionError {
    IdenticalMetadata,
    AddressNotFound,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Disconnected,
    Divergent,
    FileNotFound,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LayerNotFound,
    LinkNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    Maintenance,
    MaxHistorySearchDepth,
    NodeNotFound,
    NoRemote,
    NotALayer,
    NotALink,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NothingStaged,
    NotSupported,
    Oversized,
    PayloadNotFound,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    RevisionNotFound,
    SharedStoreNotFound,
    SlowDown,
    TokenNotFound,
    WriteRequired,
    MissingIdentity,
}

impl EventError for AmendRevisionError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct AmendRevisionOptions {
    /// Message
    pub message: Option<String>,
}

pub async fn amend_revision(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: AmendRevisionOptions,
) -> Result<Hash, AmendRevisionError> {
    amend_revision_impl(repository, token, options).await
}

async fn amend_revision_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: AmendRevisionOptions,
) -> Result<Hash, AmendRevisionError> {
    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<AmendRevisionError>("Failed to deserialize current revision anchor")?;

    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward_with::<AmendRevisionError, _>(|| {
            format!("Failed to deserialize revision state {current_revision}")
        })?;

    let metadata_hash = state_current.metadata_hash();
    let original_metadata = if metadata_hash.is_zero() {
        Metadata::new()
    } else {
        Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<AmendRevisionError>("Failed to deserialize metadata")?
    };
    let mut amended_metadata = original_metadata.clone();

    if let Some(new_message) = &options.message {
        amended_metadata
            .set_string(metadata::MESSAGE, new_message)
            .forward::<AmendRevisionError>("Failed setting revision metadata")?;
    }

    let commit_user = execution_context().user_id().await;
    if !commit_user.is_empty() {
        amended_metadata
            .set_string(metadata::COMMITTED_BY, &commit_user)
            .forward::<AmendRevisionError>("Failed setting revision metadata")?;
        let created_by_missing = amended_metadata
            .get_string(metadata::CREATED_BY)
            .map_or(true, str::is_empty);
        if created_by_missing {
            amended_metadata
                .set_string(metadata::CREATED_BY, &commit_user)
                .forward::<AmendRevisionError>("Failed setting revision metadata")?;
        }
    }

    let amended_metadata_hash = amended_metadata
        .serialize(repository.clone())
        .await
        .forward::<AmendRevisionError>("Failed to write commit metadata")?;

    if amended_metadata_hash == metadata_hash {
        return Err(IdenticalMetadata.into());
    }

    state_current.set_metadata_hash(amended_metadata_hash);

    // Serialize the new current state
    let signature = state_current
        .serialize(repository.clone(), token)
        .await
        .forward::<AmendRevisionError>("Failed to serialize revision state")?;

    store_branch_latest_and_make_current(repository.clone(), signature, current_branch)
        .await
        .forward::<AmendRevisionError>("Failed to store branch latest")?;

    event::LoreEvent::RevisionCommitRevision(LoreRevisionCommitRevisionEventData {
        repository: repository.id,
        branch: current_branch,
        revision: signature,
        revision_number: state_current.revision_number(),
        parent: state_current.parent_self(),
        parent_other: state_current.parent_other(),
    })
    .send();

    event::metadata::send(&amended_metadata);

    Ok(signature)
}
