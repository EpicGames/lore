// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Serialize;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::sync;
use crate::state;

#[error_set]
pub enum ResetError {
    Cancelled,
    LinkPinsDiverge,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    NoRemote,
    LocalModifications,
    NotAuthenticated,
    NotAuthorized,
    SlowDown,
    Maintenance,
    InvalidArguments,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
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
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for ResetError {
    fn translated(&self) -> LoreError {
        match self {
            ResetError::Disconnected(_) => LoreError::Connection,
            ResetError::SlowDown(_) => LoreError::SlowDown,
            ResetError::Oversized(_) => LoreError::Oversized,
            ResetError::FileNotFound(_) => LoreError::FileNotFound,
            ResetError::NotFound(_)
            | ResetError::BranchNotFound(_)
            | ResetError::RevisionNotFound(_) => LoreError::NotFound,
            ResetError::AddressNotFound(_) => LoreError::AddressNotFound,
            ResetError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            ResetError::InvalidPath(_) | ResetError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Event data reported when a branch is reset to a revision.
#[repr(C)]
#[derive(
    Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchResetEventData {
    /// Branch identifier.
    pub id: Context,
    /// Branch name.
    pub name: LoreString,
    /// Revision the branch was reset to.
    pub revision: Hash,
}

pub async fn reset(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: String,
    revision: String,
) -> Result<(), ResetError> {
    let context = execution_context();
    let global = context.globals();

    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<ResetError>("loading current anchor")?;

    // Reject if any node is actually staged; dirty-only tracking is
    // tolerated and forwarded by `sync::sync` → `rebase_staged_anchor`
    // when the same-branch reset path runs below.
    if let Some(staged_revision) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        && !staged_revision.is_zero()
    {
        let state_staged =
            state::State::deserialize_staged(repository.clone(), staged_revision, current_revision)
                .await
                .forward::<ResetError>("deserializing staged state")?;
        let has_staged = state_staged
            .node_has_staged_children(repository.clone(), crate::node::ROOT_NODE)
            .await
            .forward::<ResetError>("checking staged nodes")?;
        if has_staged {
            return Err(ResetError::internal(
                "Unable to reset branch when there is a staged state",
            ));
        }
    }

    let branch = if branch.is_empty() {
        current_branch.to_string()
    } else {
        branch
    };

    let by_id = Context::from_str(&branch).is_ok();
    let branch = match branch::resolve(repository.clone(), branch.as_str()).await {
        Ok(resolved) => resolved,
        Err(err) if is_refusal(&err) => {
            return Err(err).forward_any("resolving branch");
        }
        Err(err) => {
            return Err(ResetError::BranchNotFound(
                BranchNotFound {
                    branch: branch.clone(),
                }
                .chain_err_from(err, "resolving branch"),
            ));
        }
    };

    let branch_metadata = branch::metadata(repository.clone(), branch.id)
        .await
        .forward::<ResetError>("loading branch metadata")?;
    let branch_name = branch::name(&branch_metadata).unwrap_or_default();

    let branch_stack = branch::stack(&branch_metadata);
    let branch_point = if branch_stack.is_empty() {
        Hash::default()
    } else {
        branch_stack[0].revision
    };

    let resolved = revision::resolve_in_branch(
        repository.clone(),
        revision.as_str(),
        global.search_location(),
    )
    .await
    .forward::<ResetError>("resolving revision")?;
    let revision = resolved.revision;

    let state = state::State::deserialize(repository.clone(), revision)
        .await
        .forward::<ResetError>("deserializing revision state")?;
    if state.branch(repository.clone()).await != branch.id && revision != branch_point {
        return Err(ResetError::internal(
            "Given revision is not on the same branch",
        ));
    }

    if by_id && !global.dry_run() {
        branch::cache_name_mapping(repository.clone(), branch.id, branch_name).await;
    }

    let latest = branch::load_latest(repository.clone(), branch.id)
        .await
        .unwrap_or_default();
    if latest == revision {
        LoreEvent::BranchReset(LoreBranchResetEventData {
            id: branch.id,
            name: branch_name.into(),
            revision,
        })
        .send();
        return Ok(());
    }

    if current_branch == branch.id {
        let options = sync::SyncOptions {
            revision: Some(revision.to_string()),
            ..Default::default()
        };
        Box::pin(sync::sync(repository.clone(), token, options))
            .await
            .forward::<ResetError>("syncing to given revision")?;

        crate::instance::store_current_anchor(&repository, revision)
            .await
            .forward::<ResetError>("storing current anchor")?;
    }

    store_reset_latest(&repository, branch.id, revision)
        .await
        .forward::<ResetError>("storing new latest revision for branch")?;

    LoreEvent::BranchReset(LoreBranchResetEventData {
        id: branch.id,
        name: branch_name.into(),
        revision,
    })
    .send();
    Ok(())
}

/// Stores `revision` as the local latest of `branch`, divergent, since it may not be on the
/// remote branch's history, and clears the stored last synchronized revision. A reset moves the
/// latest to an arbitrary revision, so the swap is from whatever the latest holds as it is read.
///
/// A last sync at `revision`, left beside the new latest, reads as a push the peer accepted and
/// never recorded. That one is cleared before the latest is stored, so a reset stopped between
/// the two writes is never taken for such a push, and a failure to clear it fails the reset before
/// the latest is stored. Any other last sync is cleared once the latest is stored. Where another
/// command advances the latest after it is read, the swap fails with
/// [`BranchAdvanced`](branch::BranchError::BranchAdvanced), having written nothing, and the last
/// sync is left as it was, a cleared one put back as [`put_back_last_sync`] describes. Any other
/// failure may follow a latest already stored, so the last sync is cleared.
async fn store_reset_latest(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision: Hash,
) -> Result<(), branch::BranchError> {
    let stored_latest = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();
    let last_sync = branch::load_stored_last_sync(repository.clone(), branch).await;
    let cleared_first = last_sync == revision
        && branch::swap_last_sync(repository.clone(), branch, last_sync, Hash::default()).await?;
    let stored = branch::store_latest(
        repository.clone(),
        branch,
        stored_latest,
        revision,
        BranchLatestStatus::Divergent,
    )
    .await;
    let advanced = stored.as_ref().is_err_and(|err| err.is_branch_advanced());
    if advanced && cleared_first {
        put_back_last_sync(repository, branch, last_sync).await;
    } else if !advanced && !cleared_first {
        branch::store_last_sync(repository.clone(), branch, Hash::default()).await;
    }
    stored
}

/// Puts `last_sync` back as the stored last sync of `branch`, from which a reset to it cleared it
/// before its swap of the latest lost to another command. A latest at `last_sync` beside it reads
/// as a push the peer accepted and never recorded, so it stays cleared where the latest that won
/// is `last_sync`, as another reset to it leaves it, or where another command stored a last sync
/// other than zero since. A zero another command stored is not told apart from the clear, so a
/// reset elsewhere can find the last sync put back. A put-back that a latest at `last_sync`
/// overtakes is cleared again: here, where that latest lands before the second read, and by the
/// reset that stores it otherwise, which clears the last sync after its swap where it read it
/// before the put-back, and before its swap where it read it after.
async fn put_back_last_sync(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    last_sync: Hash,
) {
    let at_last_sync = async || {
        branch::load_latest(repository.clone(), branch)
            .await
            .is_ok_and(|latest| latest == last_sync)
    };
    if at_last_sync().await
        || !branch::swap_last_sync(repository.clone(), branch, Hash::default(), last_sync)
            .await
            .unwrap_or(false)
    {
        return;
    }
    if at_last_sync().await {
        let _ =
            branch::swap_last_sync(repository.clone(), branch, last_sync, Hash::default()).await;
    }
}
