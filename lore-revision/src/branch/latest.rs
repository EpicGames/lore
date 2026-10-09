// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Serialize;

use super::BranchError;
use crate::branch::load_latest_history;
use crate::event::LoreEvent;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::repository::RepositoryContext;

const DEFAULT_LATEST_LIST_LIMIT: u32 = 30;

/// Event data reported for each entry in a branch latest-revision history listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
pub struct LoreBranchLatestListEntryEventData {
    /// Branch identifier.
    pub branch: BranchId,
    /// Revision recorded in the history entry.
    pub revision: Hash,
}

pub struct ListOptions {
    pub branch: Option<String>,
    pub limit: u32,
}

pub async fn list(
    repository: Arc<RepositoryContext>,
    options: ListOptions,
) -> Result<(), BranchError> {
    let branch = if let Some(branch) = options.branch {
        super::resolve(repository.clone(), branch.as_str())
            .await?
            .id
    } else {
        crate::instance::load_current_anchor(&repository)
            .await
            .forward::<BranchError>("Failed to deserialize current revision anchor")?
            .1
    };

    let limit = if options.limit == 0 {
        DEFAULT_LATEST_LIST_LIMIT
    } else {
        options.limit
    };

    let mut current = 0;
    let mut hash = None;
    while current < limit {
        let history = load_latest_history(repository.clone(), branch, hash).await?;
        if history.revision.is_zero() {
            break;
        }
        LoreEvent::BranchLatestListEntry(LoreBranchLatestListEntryEventData {
            branch,
            revision: history.revision,
        })
        .send();
        hash = if history.previous != Hash::default() {
            Some(history.previous)
        } else {
            break;
        };
        current += 1;
    }
    Ok(())
}
