// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::LinkError;
use crate::event;
use crate::event::LoreLinkEntryEventData;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::state::State;

pub async fn list(repository: Arc<RepositoryContext>) -> Result<(), LinkError> {
    let (state_staged, parent_branch) = super::current_or_staged_state(repository.clone()).await?;

    lore_debug!("Listing links in repository");

    let mut seen = std::collections::HashSet::new();
    seen.insert(repository.id);
    Box::pin(list_recursive(
        repository,
        state_staged,
        String::new(),
        parent_branch,
        0,
        &mut seen,
    ))
    .await
}

/// Recursively enumerate links in `state`, descending into each linked
/// repository so nested links are reported with their full (top-level
/// relative) mount paths. Bounded by `MAX_LINK_DEPTH` and a visited-repository
/// set to terminate on cycles.
async fn list_recursive(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path_prefix: String,
    parent_branch: BranchId,
    link_depth: usize,
    seen: &mut std::collections::HashSet<RepositoryId>,
) -> Result<(), LinkError> {
    let link_list = state
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    for link_reference in link_list {
        let local_path = state
            .node_path(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Failed resolving link node")?;

        let local_node = state
            .node(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Specified node is not a link node")?;

        let link_context = Arc::new(repository.to_link_context(link_reference.repository).await);

        let resolved_branch = link_reference.resolve_branch(parent_branch);

        let described =
            super::describe_link(link_context.clone(), link_reference.signature, &local_node)
                .await?;

        let full_path = if path_prefix.is_empty() {
            local_path
        } else {
            format!("{path_prefix}/{local_path}")
        };

        event::LoreEvent::LinkEntry(LoreLinkEntryEventData {
            link: link_reference.repository,
            link_node: link_reference.local_node,
            link_path: full_path.clone().into(),
            source_node: local_node.child,
            source_path: described.source_path.into(),
            branch: resolved_branch,
            tracking: link_reference.is_tracking() as u8,
            revision: link_reference.signature,
            flags: link_reference.flags,
        })
        .send();

        if link_depth + 1 < crate::state::MAX_LINK_DEPTH && seen.insert(link_reference.repository) {
            Box::pin(list_recursive(
                link_context,
                described.link_state,
                full_path,
                resolved_branch,
                link_depth + 1,
                seen,
            ))
            .await?;
            seen.remove(&link_reference.repository);
        }
    }

    Ok(())
}

/// Data for an event describing a link that has staged changes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLinkStagedEntryEventData {
    /// Path of the link within the parent repository.
    pub path: LoreString,
    /// Identifier of the repository the link points to.
    pub repository: RepositoryId,
    /// Number of staged files inside the link.
    pub staged_file_count: u64,
}

#[derive(Clone, Debug)]
pub struct StagedLinkInfo {
    pub path: String,
    pub repository: RepositoryId,
    pub staged_file_count: u64,
}

pub async fn list_staged(
    repository: Arc<RepositoryContext>,
) -> Result<Vec<StagedLinkInfo>, LinkError> {
    let staged_revision = match crate::instance::load_staged_revision(&repository).await {
        Ok(Some(revision)) => revision,
        Ok(None) | Err(_) => return Ok(Vec::new()),
    };

    let state_staged = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(repository.id);
    Box::pin(list_staged_recursive(
        repository,
        state_staged,
        String::new(),
        0,
        &mut seen,
        &mut result,
    ))
    .await?;
    Ok(result)
}

/// Recursively collect staged links in `state`, descending into each linked
/// repository so a nested link with staged content is reported with its full
/// (top-level relative) mount path. Bounded by `MAX_LINK_DEPTH` and a
/// visited-repository set.
async fn list_staged_recursive(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path_prefix: String,
    link_depth: usize,
    seen: &mut std::collections::HashSet<RepositoryId>,
    result: &mut Vec<StagedLinkInfo>,
) -> Result<(), LinkError> {
    let link_list = state
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    for link_ref in &link_list {
        let node = state
            .node(repository.clone(), link_ref.local_node)
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;

        // A staged-add link has no staged content inside it yet, and an
        // unchanged link has nothing to report — but we must still descend
        // through an unchanged link to reach a nested link that did change.
        let local_path = state
            .node_path(repository.clone(), link_ref.local_node)
            .await
            .forward::<LinkError>("Failed resolving link node")?;
        let full_path = if path_prefix.is_empty() {
            local_path
        } else {
            format!("{path_prefix}/{local_path}")
        };

        let link_repository = Arc::new(repository.to_link_context(link_ref.repository).await);
        let link_state = State::deserialize(link_repository.clone(), link_ref.signature)
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;

        if node.is_staged() && !node.is_staged_add() {
            let staged_file_count = crate::state::count_staged_files(
                link_repository.clone(),
                link_state.clone(),
                node.child,
            )
            .await;

            event::LoreEvent::LinkStagedEntry(LoreLinkStagedEntryEventData {
                path: LoreString::from_str(&full_path),
                repository: link_ref.repository,
                staged_file_count,
            })
            .send();

            result.push(StagedLinkInfo {
                path: full_path.clone(),
                repository: link_ref.repository,
                staged_file_count,
            });
        }

        if link_depth + 1 < crate::state::MAX_LINK_DEPTH && seen.insert(link_ref.repository) {
            Box::pin(list_staged_recursive(
                link_repository,
                link_state,
                full_path,
                link_depth + 1,
                seen,
                result,
            ))
            .await?;
            seen.remove(&link_ref.repository);
        }
    }

    Ok(())
}
