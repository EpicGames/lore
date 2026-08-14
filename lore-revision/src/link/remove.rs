// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::errors::InvalidPath;
use crate::errors::NotALink;
use crate::event;
use crate::interface::LoreFileAction;
use crate::link::LoreLinkChangeEventData;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::stage;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

pub async fn remove(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
) -> Result<(), LinkError> {
    let (state_current, state_staged, parent_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    lore_debug!("Resolve link to unlink from {link_path}");

    // Resolve through any parent links so mutations target the owning repo.
    let chain = crate::link::resolve_link_chain(
        repository.clone(),
        state_staged.clone(),
        state_current.clone(),
        link_path.clone(),
        parent_branch,
    )
    .await?;
    let inner_repository = chain.innermost_repository.clone();
    let inner_state = chain.innermost_state.clone();

    let node_link = inner_state
        .find_relative_node_link(
            inner_repository.clone(),
            chain.innermost_base_node,
            chain.remainder_path.as_str(),
        )
        .await
        .forward::<LinkError>("Invalid path")?;

    lore_debug!("Link node is {node_link:?}");
    if !node_link.is_valid() {
        return Err(InvalidPath {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_node = inner_state
        .node(inner_repository.clone(), node_link.node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    if !link_node.is_link() {
        return Err(NotALink {
            path: link_path.to_string(),
        }
        .into());
    }

    let link_id: RepositoryId = link_node.address.context.into();
    let is_staged_add = link_node.is_staged_add();

    // On-disk path is the full link path regardless of nesting.
    let absolute_path = link_path.to_absolute_path(repository.require_path()?);
    crate::util::fs::unlink_recursive(absolute_path.as_path())
        .await
        .internal_with(|| format!("Failed to delete directory {}", absolute_path.display()))?;

    if is_staged_add {
        // Recreate the empty directory so it appears as an unstaged change
        let _ = lore_io::IoDriver::global()
            .create_dir_all(absolute_path.as_path())
            .await;

        // Link was added but never committed — discard the node from the staged tree
        lore_debug!("Link node was staged for add, discarding instead of staging delete");
        state::node_discard_patch(
            inner_state.clone(),
            inner_repository.clone(),
            node_link.node,
            |discarded_node_id, _flags| {
                lore_debug!("Discarded link node {discarded_node_id}");
            },
        )
        .await
        .forward::<LinkError>("Failed to discard link node")?;
    } else {
        // Link exists in committed state — stage as deleted
        stage::stage_delete(
            inner_repository.clone(),
            inner_state.clone(),
            node_link.node,
            NodeFlags::NoFlags,
            Arc::default(),
            None, // No link tracking when removing links
        )
        .await
        .forward::<LinkError>("Failed to delete link")?;
    }

    inner_state
        .link_remove(
            inner_repository.clone(),
            link_node.address.context.into(),
            node_link.node,
        )
        .await
        .forward::<LinkError>("Failed to remove link")?;

    // Fold nested link revisions up into the top-level state (no-op if flat).
    crate::link::propagate_link_chain(&chain, token).await?;

    state_staged.set_parent_self(state_current.revision());
    state_staged.set_revision_number(0);

    // If staged state is the initial stage based on current state, reset other parent. Otherwise
    // leave it as is, in case previous staged state was a merge/integrate
    if state_staged.revision() == state_current.revision() {
        state_staged.set_parent_other(Hash::default());
        state_staged.set_metadata_hash(Hash::default());
    }

    // Serialize the staged state
    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<LinkError>("Failed to serialize state")?;

    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<LinkError>("Failed to serialize anchor")?;

    event::LoreEvent::LinkChange(LoreLinkChangeEventData::new(
        link_path.as_str(),
        link_id,
        Context::default(),
        Hash::default(),
        LoreFileAction::Delete,
    ))
    .send();

    Ok(())
}
