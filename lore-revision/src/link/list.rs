// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::LinkError;
use crate::branch;
use crate::event;
use crate::interface::LoreString;
use crate::link::LoreLinkEntryEventData;
use crate::lore::BranchId;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_warn;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;

pub async fn list(repository: Arc<RepositoryContext>) -> Result<(), LinkError> {
    let (_state_current, state_staged, parent_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| _state_current.clone());

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

        let full_path = if path_prefix.is_empty() {
            local_path.clone()
        } else {
            format!("{path_prefix}/{local_path}")
        };

        let link = Arc::new(repository.to_link_context(link_reference.repository).await);

        let local_node = state
            .node(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Specified node is not a link node")?;

        let link_state = State::deserialize(link.clone(), link_reference.signature)
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;

        let source_path = link_state
            .node_path(link.clone(), local_node.child)
            .await
            .forward::<LinkError>("Failed resolving link node")?;

        let source_path = if source_path.is_empty() {
            String::from("/")
        } else {
            source_path
        };

        let resolved_branch = link_reference.resolve_branch(parent_branch);

        let branch_name =
            if let Ok(metadata) = branch::metadata(link.clone(), resolved_branch).await {
                branch::name(&metadata).unwrap_or_default().to_string()
            } else {
                String::new()
            };

        event::LoreEvent::LinkEntry(LoreLinkEntryEventData {
            link: link_reference.repository,
            link_node: link_reference.local_node,
            link_path: full_path.clone().into(),
            source_node: local_node.child,
            source_path: source_path.into(),
            branch: resolved_branch,
            branch_name: LoreString::from(branch_name.as_str()),
            revision: link_reference.signature,
            flags: link_reference.flags,
        })
        .send();

        if link_depth + 1 < crate::state::MAX_LINK_DEPTH && seen.insert(link_reference.repository) {
            Box::pin(list_recursive(
                link.clone(),
                link_state,
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
            let staged_file_count =
                count_staged_files(link_repository.clone(), link_state.clone(), node.child).await;

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

async fn count_staged_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> u64 {
    let mut count = 0u64;
    let children =
        match StateNodeChildrenIterator::new(state.clone(), repository.clone(), node_id).await {
            Ok(iter) => iter,
            Err(err) => {
                lore_warn!("Failed to iterate children for staged file count: {err}");
                return 0;
            }
        };

    let mut iter = children;
    while let Ok(Some((child_id, child_node))) = iter.next().await {
        if !child_node.is_staged() {
            continue;
        }
        if child_node.is_file() {
            count += 1;
        } else if child_node.is_directory() {
            count += Box::pin(count_staged_files(
                repository.clone(),
                state.clone(),
                child_id,
            ))
            .await;
        }
    }

    count
}
