// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::errors::*;
use crate::event;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::metadata::Metadata;
use crate::repository::RepositoryContext;
use crate::runtime::execution_context;
use crate::state::State;

/// Header information for a revision history listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionHistoryEventData {
    /// Repository identifier the history belongs to.
    pub repository: RepositoryId,
    /// Branch identifier the history is listed for.
    pub branch: BranchId,
}

impl LoreRevisionHistoryEventData {
    pub fn new(repository: RepositoryId, branch: BranchId) -> Self {
        Self { repository, branch }
    }
}

/// A single entry in a revision history listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionHistoryEntryEventData {
    /// Revision hash signature.
    pub revision: Hash,
    /// Revision number.
    pub revision_number: u64,
    /// Parent revision hashes; the first is the direct parent and the second
    /// is the other parent of a merge, or zero when there is none.
    pub parent: [Hash; 2],
}

impl LoreRevisionHistoryEntryEventData {
    pub fn new(state: Arc<State>) -> Self {
        Self {
            revision: state.revision(),
            revision_number: state.revision_number(),
            parent: [state.parent_self(), state.parent_other()],
        }
    }
}

#[error_set]
pub enum RevisionHistoryError {
    AddressNotFound,
    Disconnected,
    FileNotFound,
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
    MissingIdentity,
}

impl crate::event::EventError for RevisionHistoryError {}

#[derive(Clone, Debug)]
pub struct HistoryOptions {
    // Show specific revision list only
    pub revision: Option<String>,
    // Show revisions on specific branch
    pub branch: Option<String>,
    // Stop when reaching a revision created before this date
    pub date: u64,
    // Stop when this many revisions have been returned
    pub length: u32,
    // Stop when reaching a revision on a different branch (include the branch point)
    pub only_branch: bool,
}

/// Returns the start revision hash and, when identifiable, the branch context
/// to use for `only_branch` filtering. The branch is `None` when the revision
/// was given as a raw hash signature (no branch information available).
async fn find_start_revision(
    repository: Arc<RepositoryContext>,
    options: HistoryOptions,
) -> Result<(Hash, Option<Context>), RevisionHistoryError> {
    // These list options are mutually exclusive.
    if options.revision.is_some() && options.branch.is_some() {
        lore_debug!("Revision and branch are mutually exclusive options.");
        return Err(RevisionHistoryError::internal(
            "cannot specify both revision and branch in history options",
        ));
    }

    if let Some(revision) = options.revision {
        // Extract branch from "branch@number" or "branch@head" specifier
        let branch = if let Some((prefix, _)) = revision.split_once('@') {
            if prefix.is_empty() {
                // "@number" uses the current anchor branch
                crate::instance::load_current_anchor(&repository)
                    .await
                    .ok()
                    .map(|(_revision, branch)| branch)
            } else {
                branch::resolve(repository.clone(), prefix)
                    .await
                    .ok()
                    .map(|b| b.id)
            }
        } else {
            // Raw hash — no branch information available
            None
        };

        let resolved_revision = super::resolve(
            repository.clone(),
            revision,
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await;
        return Ok((
            resolved_revision.forward::<RevisionHistoryError>("resolving revision for history")?,
            branch,
        ));
    }

    if let Some(target_branch) = options.branch {
        let branch = branch::load_name_to_id(repository.clone(), target_branch)
            .await
            .internal("loading branch name")?;

        let remote_latest = if let Ok(remote) = repository.remote().await {
            branch::load_remote_latest(remote.clone(), repository.id, branch)
                .await
                .unwrap_or_default()
        } else {
            Hash::default()
        };

        if execution_context().globals().remote() {
            return Ok((remote_latest, Some(branch)));
        }

        let local_latest = branch::load_latest(repository.clone(), branch)
            .await
            .unwrap_or_default();

        return Ok((local_latest, Some(branch)));
    }

    let (anchor_signature, anchor_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<RevisionHistoryError>("deserializing current anchor")?;

    if execution_context().globals().local() {
        let local_latest = branch::load_latest(repository.clone(), anchor_branch)
            .await
            .unwrap_or_default();
        return Ok((local_latest, Some(anchor_branch)));
    }

    if execution_context().globals().remote() {
        let remote_latest = if let Ok(remote) = repository.remote().await {
            branch::load_remote_latest(remote.clone(), repository.id, anchor_branch)
                .await
                .unwrap_or_default()
        } else {
            Hash::default()
        };
        return Ok((remote_latest, Some(anchor_branch)));
    }

    Ok((anchor_signature, Some(anchor_branch)))
}

pub async fn history(
    repository: Arc<RepositoryContext>,
    options: HistoryOptions,
) -> Result<(), RevisionHistoryError> {
    let mut count = 0;
    let mut limit = if options.length > 0 {
        options.length
    } else {
        100
    };
    let (mut signature, resolved_branch) =
        find_start_revision(repository.clone(), options.clone()).await?;
    let mut start_branch: Option<Context> = if options.only_branch {
        resolved_branch
    } else {
        None
    };

    while limit > 0 && !signature.is_zero() {
        let state = State::deserialize(repository.clone(), signature)
            .await
            .forward::<RevisionHistoryError>("deserializing state")?;

        let metadata_hash = state.metadata_hash();
        let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<RevisionHistoryError>("deserializing metadata")?;

        // Check if we've crossed a date boundary
        if options.date != 0
            && let Ok(ts) = metadata.get_timestamp()
            && ts < options.date
        {
            break;
        }

        // Check if we've crossed a branch boundary
        let crossed_branch = if options.only_branch
            && let Ok(branch) = metadata.get_branch()
        {
            if let Some(ref start) = start_branch {
                branch != *start
            } else {
                start_branch = Some(branch);
                false
            }
        } else {
            false
        };

        if count == 0 {
            let branch = if options.branch.is_some() {
                // Take branch from given arguments
                branch::load_name_to_id(
                    repository.clone(),
                    options.branch.as_deref().unwrap_or_default(),
                )
                .await
                .internal("loading branch name")?
            } else {
                // Take branch from top revision.
                metadata
                    .get_branch()
                    .internal("getting branch from metadata")?
            };

            event::LoreEvent::RevisionHistory(LoreRevisionHistoryEventData::new(
                repository.id,
                branch,
            ))
            .send();
        }

        event::LoreEvent::RevisionHistoryEntry(LoreRevisionHistoryEntryEventData::new(
            state.clone(),
        ))
        .send();

        if !metadata_hash.is_zero() {
            event::metadata::send(&metadata).internal("sending metadata event")?;
        }

        if crossed_branch {
            break;
        }

        count += 1;
        limit -= 1;
        signature = state.parent_self();
    }

    Ok(())
}
