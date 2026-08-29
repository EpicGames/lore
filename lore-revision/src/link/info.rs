// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::branch;
use crate::errors::InvalidPath;
use crate::errors::NotALink;
use crate::event;
use crate::event::LoreLinkEntryEventData;
use crate::event::LoreLinkInfoEventData;
use crate::event::LoreLinkStagedState;
use crate::interface::LoreString;
use crate::link;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util::path::RelativePath;

pub async fn info(
    repository: Arc<RepositoryContext>,
    link_path: RelativePath,
) -> Result<(), LinkError> {
    let (state_current, state_staged, parent_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    lore_debug!("Resolving link at {link_path}");

    // Resolve through any parent links so the branch resolves against the
    // innermost crossed link's branch rather than the top-level one.
    let chain = link::resolve_link_chain(
        repository.clone(),
        state_staged.clone(),
        state_current.clone(),
        link_path.clone(),
        parent_branch,
    )
    .await?;

    let owner_repository = chain.innermost_repository.clone();
    let owner_state = chain.innermost_state.clone();
    let owner_branch = chain
        .levels
        .last()
        .map_or(parent_branch, |level| level.branch);

    let node_link = owner_state
        .find_relative_node_link(
            owner_repository.clone(),
            chain.innermost_base_node,
            chain.remainder_path.as_str(),
        )
        .await
        .forward::<LinkError>("Invalid path")?;

    if !node_link.is_valid() {
        return Err(InvalidPath {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_node = owner_state
        .node(owner_repository.clone(), node_link.node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    if !link_node.is_link() {
        return Err(NotALink {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_context = Arc::new(
        owner_repository
            .to_link_context(link_node.address.context.into())
            .await,
    );

    // Staging a removal drops the registry entry but leaves the node in the
    // tree, so fall back to the committed registry to describe a link on its
    // way out.
    let link_reference = match owner_state
        .link_find(owner_repository.clone(), link_context.id, node_link.node)
        .await
    {
        Ok(reference) => reference,
        Err(_) => chain
            .innermost_current_state
            .link_find(owner_repository.clone(), link_context.id, node_link.node)
            .await
            .forward::<LinkError>("Failed to find link")?,
    };

    let resolved_branch = link_reference.resolve_branch(owner_branch);

    let described =
        link::describe_link(link_context.clone(), link_reference.signature, &link_node).await?;

    // Delete before add, matching `Node::staged_action`: a link staged for
    // addition and then for deletion reports the deletion.
    let staged_state = if link_node.is_staged_delete() {
        LoreLinkStagedState::Removed
    } else if link_node.is_staged_add() {
        LoreLinkStagedState::Added
    } else if link_node.is_staged() {
        LoreLinkStagedState::Modified
    } else {
        LoreLinkStagedState::None
    };

    // A link staged for addition has no committed content inside it yet, and one
    // staged for deletion is going away with everything in it.
    let staged_file_count = if matches!(staged_state, LoreLinkStagedState::Modified) {
        crate::state::count_staged_files(
            link_context.clone(),
            described.link_state.clone(),
            link_node.child,
        )
        .await
    } else {
        0
    };

    let remote_revision = remote_latest(&link_context, resolved_branch)
        .await
        .unwrap_or_default();

    event::LoreEvent::LinkInfo(LoreLinkInfoEventData {
        entry: LoreLinkEntryEventData {
            link: link_context.id,
            link_node: node_link.node,
            link_path: LoreString::from(link_path.as_str()),
            source_node: link_node.child,
            source_path: LoreString::from(described.source_path.as_str()),
            branch: resolved_branch,
            tracking: link_reference.is_tracking() as u8,
            revision: link_reference.signature,
            flags: link_reference.flags,
        },
        remote_revision,
        staged_state,
        staged_file_count,
    })
    .send();

    Ok(())
}

/// `None` when the remote is not consulted or unreachable.
async fn remote_latest(link_context: &Arc<RepositoryContext>, branch: BranchId) -> Option<Hash> {
    if execution_context().globals().offline_or_local() {
        return None;
    }
    let remote = link_context.remote().await.ok()?;
    branch::load_remote_latest(remote, link_context.id, branch)
        .await
        .ok()
}
