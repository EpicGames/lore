// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::interface::LoreError;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::sync;
use crate::state::State;
use crate::util::serde::u8_as_bool;

/// Progress of a bisect search across a range of revisions.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionBisectEventData {
    /// Revision number at the start of the search range.
    pub start_revision_number: u64,
    /// Revision number selected to test next.
    pub target_revision_number: u64,
    /// Revision number at the end of the search range.
    pub end_revision_number: u64,
    /// Flag indicating the search has finished.
    #[serde(with = "u8_as_bool")]
    pub done: u8,
}

#[error_set]
pub enum BisectError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    RevisionNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    NoRemote,
    LocalModifications,
    Disconnected,
    NotAuthenticated,
    NotAuthorized,
    SlowDown,
    Maintenance,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    InvalidArguments,
    LayerNotFound,
    LinkPathNotFound,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NotConnected,
    NothingStaged,
    NotSupported,
    PayloadNotFound,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for BisectError {
    fn translated(&self) -> LoreError {
        match self {
            Self::Disconnected(_) => LoreError::Connection,
            Self::SlowDown(_) => LoreError::SlowDown,
            Self::Oversized(_) => LoreError::Oversized,
            Self::NotFound(_) => LoreError::NotFound,
            Self::AddressNotFound(_) => LoreError::AddressNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug, Default)]
pub struct BisectOptions {
    pub start: String,
    pub end: String,
}
pub async fn bisect(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: BisectOptions,
) -> Result<(), BisectError> {
    Box::pin(bisect_impl(repository, token, options)).await
}

async fn bisect_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: BisectOptions,
) -> Result<(), BisectError> {
    let BisectOptions { start, end } = options;
    let start_revision = revision::resolve(
        repository.clone(),
        &start,
        execution_context().globals().search_limit(),
        execution_context().globals().search_location(),
    )
    .await
    .forward::<BisectError>("resolving start revision")?;

    lore_debug!("Bisect resolved start revision target is {start_revision}");

    let end_revision = revision::resolve(
        repository.clone(),
        &end,
        execution_context().globals().search_limit(),
        execution_context().globals().search_location(),
    )
    .await
    .forward::<BisectError>("resolving end revision")?;
    lore_debug!("Bisect resolved end revision target is {end_revision}");

    let start_state = State::deserialize(repository.clone(), start_revision)
        .await
        .forward::<BisectError>("deserializing start state")?;
    let end_state = State::deserialize(repository.clone(), end_revision)
        .await
        .forward::<BisectError>("deserializing end state")?;

    let search_limit = execution_context()
        .globals()
        .search_limit()
        .unwrap_or(usize::MAX);
    let steps_count = {
        let mut steps = 0;
        let mut revision = end_revision;

        let start_revision_number = start_state.revision_number();
        loop {
            let state = State::deserialize(repository.clone(), revision)
                .await
                .forward::<BisectError>("deserializing state during range scan")?;

            lore_debug!(
                "Calculating range between revisions. Revision number {}",
                state.revision_number()
            );
            if state.revision_number() < start_revision_number {
                return Err(BisectError::internal(
                    "Failed to find path between start and end revisions",
                ));
            }

            revision = state.parent_self();
            steps += 1;

            if revision.is_zero() {
                return Err(BisectError::internal(
                    "Failed to find path between start and end revisions",
                ));
            } else if revision == start_revision {
                break;
            }

            if steps > search_limit {
                return Err(BisectError::internal(
                    "Search limit encountered while traversing between start and end revisions",
                ));
            }
        }

        steps
    };

    let target_offset = steps_count / 2;

    let target_revision = {
        let mut revision = end_revision;
        for _ in 0..target_offset {
            let state = State::deserialize(repository.clone(), revision)
                .await
                .forward::<BisectError>("deserializing state during target search")?;

            revision = state.parent_self();
            lore_debug!(
                "Finding target revision. Revision number: {}",
                state.revision_number()
            );
        }

        revision
    };

    lore_debug!("Bisect target revision is {target_revision}");
    lore_debug!("Bisect is syncing to target revision");

    Box::pin(sync(
        repository.clone(),
        token,
        SyncOptions {
            revision: Some(target_revision.to_string()),
            ..Default::default()
        },
    ))
    .await
    .forward::<BisectError>("syncing to target revision")?;

    let target_state = State::deserialize(repository.clone(), target_revision)
        .await
        .forward::<BisectError>("deserializing target state")?;

    LoreEvent::RevisionBisect(LoreRevisionBisectEventData {
        start_revision_number: start_state.revision_number(),
        target_revision_number: target_state.revision_number(),
        end_revision_number: end_state.revision_number(),
        done: if steps_count <= 1 {
            1
        } else {
            Default::default()
        },
    })
    .send();

    Ok(())
}
