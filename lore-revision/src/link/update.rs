// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::branch;
use crate::errors::InvalidPath;
use crate::errors::NotALink;
use crate::event;
use crate::filter::FilterMode;
use crate::interface::LoreFileAction;
use crate::link;
use crate::link::LoreLinkChangeEventData;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::stage;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

pub async fn update(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
    pin: Option<String>,
) -> Result<(), LinkError> {
    let (state_current, state_staged, parent_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    lore_debug!("Resolve link to update at path {link_path}");

    // Resolve through any parent links so mutations target the owning repo.
    let chain = link::resolve_link_chain(
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

    if link_node.is_staged() {
        return Err(LinkError::internal("Link has staged changes"));
    }

    let linked_node = link_node.child;

    // TODO(vri): Verify filesystem in any case for local modifications
    // Tree roots at the innermost node; filesystem path is the full link path.
    if state_current.revision() != state_staged.revision() {
        let (linked_changes, _changes_stats) = state::diff_filesystem_subtree(
            inner_repository.clone(),
            inner_state.clone(),
            inner_repository.clone(),
            inner_state.clone(),
            link_path.clone(),
            node_link.node,
            node_link.node,
            FilterMode::View,
            std::sync::Arc::new(Vec::new()),
        )
        .await
        .forward::<LinkError>("Failed to diff link with filesystem")?;

        if !linked_changes.is_empty() {
            return Err(LinkError::internal("Link has filesystem changes"));
        }
    }

    let link = Arc::new(
        repository
            .to_link_context(link_node.address.context.into())
            .await,
    );
    let link_remote = link.remote().await.forward::<LinkError>("Not connected")?;
    let link_reference = inner_state
        .link_find(inner_repository.clone(), link.id, node_link.node)
        .await
        .forward::<LinkError>("Failed to find link")?;

    let current_link_revision = link_reference.signature;
    let resolved_branch = link_reference.resolve_branch(parent_branch);

    // If a pin is specified, attempt to resolve it
    let (link_revision, link_branch) = if let Some(pin) = pin {
        link::resolve_pin(link.clone(), pin).await?
    } else {
        let link_latest = branch::load_remote_latest(link_remote.clone(), link.id, resolved_branch)
            .await
            .forward::<LinkError>("Failed to load link latest")?;

        lore_debug!("Using latest {link_latest} of current branch {resolved_branch} as link pin");

        (link_latest, resolved_branch)
    };

    lore_debug!("Updating to link revision {link_revision} on branch {link_branch}");

    // Link is unchanged
    if current_link_revision == link_revision && resolved_branch == link_branch {
        event::LoreEvent::LinkChange(LoreLinkChangeEventData::new(
            link_path.as_str(),
            link.id,
            Context::default(),
            Hash::default(),
            LoreFileAction::Keep,
        ))
        .send();

        return Ok(());
    }

    let mut node = link_node;
    node.address.hash = link_revision;

    lore_debug!("Staging link node");
    let link_node = stage::stage_single_node(
        inner_repository.clone(),
        inner_state.clone(),
        chain.remainder_path.clone().freeze(),
        node,
        Arc::default(),
        None, // No link tracking when updating links
        FilterMode::View,
    )
    .await
    .forward::<LinkError>("Failed staging the link node")?;

    inner_state
        .link_update(
            inner_repository.clone(),
            link.id,
            link_branch,
            link_revision,
            link_node.node,
        )
        .await
        .forward::<LinkError>("Failed to update link")?;

    // Filesystem realize uses the full mount path.
    link::realize_link_pin_change(
        repository.clone(),
        link.clone(),
        link_path.clone(),
        current_link_revision,
        link_revision,
        linked_node,
    )
    .await?;

    // Fold nested link revisions up into the top-level state (no-op if flat).
    link::propagate_link_chain(&chain, token).await?;

    state_staged.set_parent_self(state_current.revision());

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
        link.id,
        link_branch,
        link_revision,
        LoreFileAction::Keep,
    ))
    .send();

    Ok(())
}
