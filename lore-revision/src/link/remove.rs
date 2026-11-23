// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::errors::LocalModifications;
use crate::event;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::fs::realize::MountedTree;
use crate::fs::realize::open_mounted_tree;
use crate::interface::LoreFileAction;
use crate::link::LoreLinkChangeEventData;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_warn;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::stage;
use crate::state;
use crate::state::NodeMapping;
use crate::state::State;
use crate::util::path::RelativePath;

/// Removes the link at `link_path` and the files it tracks in its mount, which a local change to
/// any of them refuses unless forced. A provider holding its state serves the link from its node,
/// so nothing at or below the mount is read or written
/// ([`holds_state`](crate::fs::filesystem_provider::FilesystemProvider::holds_state)).
pub(crate) async fn remove(
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

    let link_id: RepositoryId = link_node.address.context.into();
    let is_staged_add = link_node.is_staged_add();
    // The tree's spelling, which the working tree holds the mount under however the caller
    // spelled it.
    let mount = match inner_state
        .node_path_below(inner_repository.clone(), link_node_id, chain.innermost.node)
        .await
    {
        Ok(Some(below)) => chain.innermost.path.join(below.as_str()),
        _ => link_path.clone(),
    };

    let shown = if repository.file_system().holds_state() {
        None
    } else {
        open_mounted_tree(&inner_repository, &link_node).await
    };
    // One operation covers the removal: the working tree read to find local changes is the one
    // the mount is then deleted from, and the staged state is stored before it finalizes, as an
    // SWFS instance serves links from what is stored then (ADR-00042).
    with_operation::<_, LinkError, _>(repository.file_system(), async |operation| {
        if !operation.holds_state() {
            if !execution_context().globals().force() {
                verify_no_local_changes_under_link(&operation, shown.clone(), &mount).await?;
            }
            remove_link_mount(&operation, &repository, &mount, shown, is_staged_add).await?;
        }

        if is_staged_add {
            // Link was added but never committed — discard the node from the staged tree
            lore_debug!("Link node was staged for add, discarding instead of staging delete");
            state::node_discard_fused(inner_state.clone(), inner_repository.clone(), link_node_id)
                .await
                .forward::<LinkError>("Failed to discard link node")?;
        } else {
            // Link exists in committed state — stage as deleted
            stage::stage_delete(
                inner_repository.clone(),
                inner_state.clone(),
                link_path.clone(),
                link_node_id,
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
                link_node_id,
            )
            .await
            .forward::<LinkError>("Failed to remove link")?;

        // Fold nested link revisions up into the top-level state (no-op if flat).
        crate::link::propagate_link_chain(&chain, token, crate::link::LinkPinRealize::StateOnly)
            .await?;

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

/// Boxed version of [`remove`] for cross-crate use.
pub fn remove_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
) -> crate::BoxFuture<'_, Result<(), LinkError>> {
    Box::pin(remove(repository, token, link_path))
}

/// Removing a link deletes from disk the files it tracks in its mount, which `shown` maps, taking
/// their uncommitted changes with no way back. What it does not track stays. A file under a case
/// variant of its name holding what the link holds is no such change.
pub(crate) async fn verify_no_local_changes_under_link(
    operation: &Arc<InstanceOperationImpl>,
    shown: Option<MountedTree>,
    link_path: &RelativePath,
) -> Result<(), LinkError> {
    let work = state::link_mount_work(operation, shown, link_path, false, |_| false)
        .await
        .forward::<LinkError>("Failed comparing link content with the file system")?;
    if let Some(path) = &work.unknown {
        lore_warn!(
            "Link at '{link_path}' holds what cannot be read (use --force to discard it): {path}"
        );
        return Err(LocalModifications.into());
    }
    if work.modified_count > 0 {
        lore_warn!(
            "Link at '{link_path}' has locally modified files (use --force to discard them): {}",
            work.named()
        );
        return Err(LocalModifications.into());
    }
    Ok(())
}

/// Deletes the files the link tracks in its mount, which `shown` maps, and the directories they
/// leave empty, and leaves an empty directory behind where the link was only ever staged for add.
///
/// The mount sits at the full link path regardless of nesting, the spelling the operation
/// resolves. A link staged for add replaced whatever the path held, so removing it leaves the
/// path reporting as an unstaged change rather than as a deletion of content the repository
/// never committed.
async fn remove_link_mount(
    operation: &InstanceOperationImpl,
    repository: &RepositoryContext,
    link_path: &RelativePath,
    shown: Option<MountedTree>,
    is_staged_add: bool,
) -> Result<(), LinkError> {
    crate::fs::realize::remove_link_mount(
        operation,
        &repository.filter,
        repository.path(),
        link_path,
        shown,
        &[],
    )
    .await
    .forward_with::<LinkError, _>(|| format!("Failed to delete directory {link_path}"))?
    .report(link_path);

    if is_staged_add {
        let _ = operation.create_dir_all(link_path).await;
    }

    Ok(())
}
