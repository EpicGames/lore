// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::branch;
use crate::errors::Divergent;
use crate::errors::LinkNotFound;
use crate::event;
use crate::find;
use crate::find::HistoryPosition;
use crate::fs::filesystem_provider::with_operation;
use crate::interface::LoreFileAction;
use crate::link;
use crate::link::LoreLinkChangeEventData;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_warn;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state::Mark;
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
    let (link_node_id, link_node) = chain.innermost_link(&link_path).await?;
    lore_debug!("Link node is {link_node_id}");

    let link = repository
        .to_link_context(link_node.address.context.into())
        .await;
    let unreachable = || link::unreachable(link.id);
    let rows: Vec<_> = inner_state
        .link_list(inner_repository.clone())
        .await
        .forward::<LinkError>("Failed to find link")?
        .into_iter()
        .filter(|row| row.repository() == link.id)
        .collect();
    let link_reference = *rows
        .iter()
        .find(|row| row.local_node() == link_node_id)
        .ok_or(LinkNotFound)?;
    for row in &rows {
        if inner_state
            .node(inner_repository.clone(), row.local_node())
            .await
            .forward::<LinkError>("Failed deserializing state")?
            .is_staged()
        {
            return Err(LinkError::internal("Link has staged changes"));
        }
    }

    let link_remote = link
        .remote()
        .await
        .forward_with::<LinkError, _>(unreachable)?;

    let current_link_revision = link_reference.signature();
    let owner_branch = chain.owner_branch(parent_branch);
    let resolved_branch = link_reference.resolve_branch(owner_branch);

    // If a pin is specified, attempt to resolve it
    let (link_revision, link_branch) = if let Some(pin) = pin {
        link::resolve_pin(link.clone(), pin)
            .await
            .forward_with::<LinkError, _>(unreachable)?
    } else {
        let link_latest = branch::load_remote_latest(link_remote.clone(), link.id, resolved_branch)
            .await
            .forward_with::<LinkError, _>(unreachable)?;

        lore_debug!("Using latest {link_latest} of current branch {resolved_branch} as link pin");

        // A zero remote latest, or a walk that cannot decide, updates as before.
        let position = if link_latest.is_zero()
            || link_latest == current_link_revision
            || execution_context().globals().force()
        {
            None
        } else {
            find::try_history_position(&link, current_link_revision, link_latest)
                .await
                .inspect_err(|err| {
                    lore_debug!(
                        "Pin {current_link_revision} not placed against {link_latest}: {err}"
                    );
                })
                .ok()
        };
        let link_revision = match position {
            None | Some(HistoryPosition::OnHistory) => link_latest,
            Some(HistoryPosition::Ahead) => {
                let branch = branch::display_name(link.clone(), resolved_branch).await;
                lore_info!(
                    "Link '{link_path}' is ahead of remote branch '{branch}': its pin {current_link_revision} holds revisions not pushed yet. Push to publish them."
                );
                current_link_revision
            }
            Some(HistoryPosition::Diverged) => {
                let branch = branch::display_name(link.clone(), resolved_branch).await;
                lore_warn!(
                    "Link '{link_path}' and remote branch '{branch}' have diverged: the pin {current_link_revision} holds revisions not pushed, and the remote branch has new ones. Updating would drop the unpushed revisions from the working tree; they stay reachable as {current_link_revision}. To drop them, run `lore --force link update {link_path}`, or pick a revision with `lore link update {link_path} --pin <revision>`."
                );
                return Err(LinkError::from(Divergent)).forward_with::<LinkError, _>(|| {
                    format!("updating the link at {link_path} to the latest of branch {branch}")
                });
            }
        };
        (link_revision, resolved_branch)
    };

    lore_debug!("Updating to link revision {link_revision} on branch {link_branch}");

    let stored_branch = link_reference.branch_to_store(link_branch, owner_branch);
    // Link is unchanged
    if rows.iter().all(|row| {
        row.signature() == link_revision
            && row.branch() == stored_branch
            && row.flags() == link_reference.flags()
    }) && resolved_branch == link_branch
    {
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

    let moved = link::repin_repository(
        &inner_state,
        inner_repository.clone(),
        link.id,
        link_revision,
        stored_branch,
    )
    .await
    .forward::<LinkError>("Failed to update link")?;
    if moved.is_empty() {
        // Only the branch changes, so nothing below reads the pin; reading it here still refuses
        // a repository the user can't reach.
        State::deserialize(link.clone(), link_revision)
            .await
            .forward_with::<LinkError, _>(unreachable)?;
    }

    let mut pins = Vec::with_capacity(moved.len());
    let mut mount_paths = Vec::with_capacity(rows.len());
    for row in &rows {
        inner_state
            .node_mark(
                &inner_repository,
                row.local_node(),
                Mark::staged(NodeFlags::Staged, true),
            )
            .await
            .forward::<LinkError>("Failed staging the link node")?;
        let Some(below) = inner_state
            .node_path_below(
                inner_repository.clone(),
                row.local_node(),
                chain.innermost.node,
            )
            .await
            .forward::<LinkError>("Failed resolving the path of a mount")?
        else {
            continue;
        };
        let mount_path = chain.innermost.path.join(below.as_str());
        if let Some(mount) = moved
            .iter()
            .find(|mount| mount.link_node == row.local_node())
        {
            pins.push((link.clone(), mount.pin(mount_path.clone())));
        }
        mount_paths.push(mount_path);
    }

    // The staged state is stored before the operation that realizes the new pin finalizes: an
    // SWFS instance serves the link from what is stored then (ADR-00042).
    with_operation::<_, LinkError, _>(repository.file_system(), async |operation| {
        link::realize_resolved_link_pins_in_operation(&operation, repository.clone(), &pins)
            .await?;

        // Fold nested link revisions up into the top-level state (no-op if flat).
        link::propagate_link_chain(&chain, token, link::LinkPinRealize::StateOnly).await?;
        link::serve_outermost_link(&operation, &chain).await?;

        state_staged.reparent_onto(state_current.revision());

        // Serialize the staged state
        let signature = state_staged
            .serialize(repository.clone(), token)
            .await
            .forward::<LinkError>("Failed to serialize state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<LinkError>("Failed to serialize anchor")
    })
    .await?;

    for mount_path in &mount_paths {
        event::LoreEvent::LinkChange(LoreLinkChangeEventData::new(
            mount_path.as_str(),
            link.id,
            link_branch,
            link_revision,
            LoreFileAction::Keep,
        ))
        .send();
    }

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
