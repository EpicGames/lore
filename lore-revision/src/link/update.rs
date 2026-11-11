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
use crate::state::NodeMapping;
use crate::state::State;
use crate::util::path::RelativePath;

pub(crate) async fn update(
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
        NodeMapping::root(repository.clone(), state_staged.clone()),
        state_current.clone(),
        link_path.clone(),
        parent_branch,
    )
    .await?;
    let inner_repository = chain.innermost.repository.clone();
    let inner_state = chain.innermost.state.clone();

    let node_link = inner_state
        .find_relative_node_link(
            inner_repository.clone(),
            chain.innermost.node,
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

    let link = repository
        .to_link_context(link_node.address.context.into())
        .await;
    let link_remote = link.remote().await.forward::<LinkError>("Not connected")?;
    let link_reference = inner_state
        .link_find(inner_repository.clone(), link.id, node_link.node)
        .await
        .forward::<LinkError>("Failed to find link")?;

    let current_link_revision = link_reference.signature;
    let owner_branch = chain.owner_branch(parent_branch);
    let resolved_branch = link_reference.resolve_branch(owner_branch);

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

    // The pinned revision numbers its nodes afresh, so the mount is carried across by path.
    let pin = link::ResolvedLinkPin::resolve(
        &link,
        link::LinkPinRealizeTarget {
            link_path: link_path.clone(),
            old_signature: current_link_revision,
            new_signature: link_revision,
            current_node: linked_node,
            mount_source: link::LinkMountSource::SamePath,
        },
    )
    .await?;

    let mut node = link_node;
    node.address.hash = link_revision;
    node.child = pin.node_target;

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
            link_reference.branch_to_store(link_branch, owner_branch),
            link_revision,
            link_node.node,
        )
        .await
        .forward::<LinkError>("Failed to update link")?;

    // Filesystem realize uses the full mount path.
    link::realize_resolved_link_pin(repository.clone(), link.clone(), &pin).await?;

    // Fold nested link revisions up into the top-level state (no-op if flat).
    link::propagate_link_chain(&chain, token).await?;

    state_staged.reparent_onto(state_current.revision());

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

/// Boxed version of [`update`] for cross-crate use.
pub fn update_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
    pin: Option<String>,
) -> crate::BoxFuture<'_, Result<(), LinkError>> {
    Box::pin(update(repository, token, link_path, pin))
}
