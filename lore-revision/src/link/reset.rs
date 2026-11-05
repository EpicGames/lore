// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::errors::LocalModifications;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::link;
use crate::link::LinkFlags;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::ROOT_NODE;
use crate::state::NodeMapping;
use crate::state::OwnFlags;
use crate::state::State;

/// Removes a staged link add from the registry and the link's files from the mount, where they
/// hold no local change unless `overwrite_local` says to discard it. Files the link does not hold
/// stay, and with them the mount.
pub(crate) async fn reset_staged_add_link(
    operation: &Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    staged_link_node: Node,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let NodeMapping {
        repository,
        state: state_staged,
        path: link_path,
        node: link_node_id,
    } = at;
    let link_id = staged_link_node.linked_node().repository;
    let absolute_path = link_path
        .to_absolute_path(repository.require_path()?)
        .forward::<LinkError>("placing the link below the root")?;
    let dry_run = execution_context().globals().dry_run();

    if !dry_run {
        link::realize_link_pin_change_in_operation(
            operation,
            repository.clone(),
            repository.to_link_context(link_id).await,
            link::LinkPinRealizeTarget {
                link_path: link_path.clone(),
                old_signature: staged_link_node.address.hash,
                new_signature: Hash::default(),
                current_node: staged_link_node.child,
                mount_source: link::LinkMountSource::Node(ROOT_NODE),
            },
            overwrite_local,
        )
        .await?;
    }

    // If the link replaced a committed directory, `link::add` staged that
    // directory node for delete. Capture it so we can restore it. Resolve
    // against the link node's path WITHIN its owning repository (not the full
    // top-level `link_path`), so this works for a nested link whose owning
    // state is a linked repo rather than the top-level one.
    let owner_relative_path = state_staged
        .node_path(repository.clone(), link_node_id)
        .await
        .unwrap_or_else(|_| link_path.as_str().to_string());
    let committed_directory_node = state_current
        .find_node_link(repository.clone(), &owner_relative_path)
        .await
        .ok()
        .filter(|node_link| node_link.is_valid())
        .map(|node_link| node_link.node);

    state_staged
        .link_remove(repository.clone(), link_id, link_node_id)
        .await
        .forward::<LinkError>("Failed to remove link registry entry")?;

    if let Some(committed_node_id) = committed_directory_node {
        // Restore the committed directory: recreate the empty placeholder on
        // disk and clear the staged-delete on its node so it returns to its
        // clean committed state.
        if !dry_run {
            lore_io::IoDriver::global()
                .create_dir_all(absolute_path.as_path())
                .await
                .internal("recreating the placeholder directory")?;
        }

        let block = state_staged
            .block(repository.clone(), NodeBlock::index(committed_node_id))
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;
        state_staged.node_clear_own(&block, committed_node_id, OwnFlags::Change);

        return Ok(());
    }

    if dry_run || operation.remove(&link_path).await.is_err() {
        return Ok(());
    }

    let mut current_buf = link_path.into_buf();
    loop {
        current_buf.pop();
        if current_buf.as_str().is_empty() {
            break;
        }
        let parent_abs = repository.require_path()?.join(current_buf.as_str());
        if lore_io::IoDriver::global()
            .remove_dir(parent_abs.as_path())
            .await
            .is_err()
        {
            break;
        }
    }

    Ok(())
}

/// Restores the link registry entry a staged removal dropped and re-realizes the pinned content
/// at the mount, refusing to replace a file standing there or a local change to a file the link
/// holds unless `overwrite_local` says to discard it.
///
/// Realizes through `operation`, the caller's: a filesystem holds one operation at a time and
/// unstaging opens one for the whole walk.
pub(crate) async fn reset_staged_remove_link(
    operation: &Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    current_link_node: Node,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let NodeMapping {
        repository,
        state: state_staged,
        path: link_path,
        node: link_node_id,
    } = at;
    let link_id = current_link_node.linked_node().repository;

    let current_link_ref = state_current
        .link_find(repository.clone(), link_id, link_node_id)
        .await
        .forward::<LinkError>("Failed to find link registry entry")?;

    if operation
        .file_info(&link_path)
        .await
        .forward::<LinkError>("Failed to check the link path")?
        .is_file()
    {
        if !overwrite_local {
            lore_warn!(
                "Link at '{link_path}' has local changes (use `lore reset {link_path}` to discard them)"
            );
            return Err(LocalModifications.into());
        }
        operation
            .remove(&link_path)
            .await
            .forward::<LinkError>("Failed to remove the file at the link path")?;
    }

    let linked_repository = repository.to_link_context(link_id).await;

    // A mount added while this removal was staged can overlap the one being
    // restored.
    let source_path = link::pinned_source_path(linked_repository.clone(), &current_link_node)
        .await
        .forward::<LinkError>("Failed resolving link source path")?;
    link::check_source_path_overlap(
        &state_staged,
        repository.clone(),
        linked_repository.clone(),
        source_path,
        link_node_id,
    )
    .await
    .forward::<LinkError>("Failed checking link source paths")?;

    state_staged
        .link_add(
            repository.clone(),
            current_link_ref.repository,
            current_link_ref.branch,
            current_link_ref.signature,
            current_link_ref.local_node,
            LinkFlags::from_bits_truncate(current_link_ref.flags),
        )
        .await
        .forward::<LinkError>("Failed to restore link registry entry")?;

    operation
        .create_dir_all(&link_path)
        .await
        .forward::<LinkError>("Failed to recreate the link directory")?;

    link::realize_link_pin_change_in_operation(
        operation,
        repository.clone(),
        linked_repository,
        link::LinkPinRealizeTarget {
            link_path,
            old_signature: Hash::default(),
            new_signature: current_link_node.address.hash,
            current_node: current_link_node.child,
            mount_source: link::LinkMountSource::SamePath,
        },
        overwrite_local,
    )
    .await?;

    Ok(())
}

/// Puts a link whose pin or mount move was staged back to the pin and mount the current revision
/// holds, on disk and in the registry.
///
/// Realizes through the caller's `operation`, as [`reset_staged_remove_link`] does.
pub(crate) async fn reset_staged_update_link(
    operation: &Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    staged_link_node: Node,
    current_link_node: Node,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let NodeMapping {
        repository,
        state: state_staged,
        path: link_path,
        node: link_node_id,
    } = at;
    let link_id = current_link_node.linked_node().repository;
    let staged_pin = staged_link_node.address.hash;
    let current_pin = current_link_node.address.hash;

    let prev_ref = state_current
        .link_find(repository.clone(), link_id, link_node_id)
        .await
        .forward::<LinkError>("Failed to find link registry entry")?;

    let linked_repository = repository.to_link_context(link_id).await;
    link::realize_link_pin_change_in_operation(
        operation,
        repository.clone(),
        linked_repository,
        link::LinkPinRealizeTarget {
            link_path,
            old_signature: staged_pin,
            new_signature: current_pin,
            current_node: staged_link_node.child,
            mount_source: link::LinkMountSource::Node(current_link_node.child),
        },
        overwrite_local,
    )
    .await?;

    state_staged
        .link_update(
            repository,
            link_id,
            prev_ref.branch,
            current_pin,
            link_node_id,
        )
        .await
        .forward::<LinkError>("Failed to update link registry entry")?;

    Ok(())
}
