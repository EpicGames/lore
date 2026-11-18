// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::errors::LocalModifications;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::realize::open_mounted_tree;
use crate::link;
use crate::link::LinkFlags;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::LinkReference;
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
    drop_staged_link(operation, &at, &staged_link_node, overwrite_local).await?;
    let NodeMapping {
        repository,
        state: state_staged,
        path: link_path,
        node: link_node_id,
    } = at;
    let absolute_path = link_path
        .to_absolute_path(repository.require_path()?)
        .forward::<LinkError>("placing the link below the root")?;
    let dry_run = execution_context().globals().dry_run();

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

/// Removes the files and the registry row of the staged link `staged_link_node` at the mount
/// `at`, refusing a local change to a file the link holds unless `overwrite_local`. A dry run
/// leaves the files.
async fn drop_staged_link(
    operation: &Arc<InstanceOperationImpl>,
    at: &NodeMapping,
    staged_link_node: &Node,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let link_id = staged_link_node.linked_node().repository;
    if !execution_context().globals().dry_run() {
        link::realize_link_pin_change_boxed(
            operation,
            at.repository.clone(),
            at.repository.to_link_context(link_id).await,
            link::LinkPinRealizeTarget {
                link_path: at.path.clone(),
                old_signature: staged_link_node.address.hash,
                new_signature: Hash::default(),
                current_node: staged_link_node.child,
                mount_source: link::LinkMountSource::Node(ROOT_NODE),
            },
            overwrite_local,
        )
        .await?;
    }

    at.state
        .link_remove(at.repository.clone(), link_id, at.node)
        .await
        .forward::<LinkError>("Failed to remove link registry entry")
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
    let restorable = restorable_link(&at, &state_current, &current_link_node).await?;
    restore_link(
        operation,
        at,
        restorable,
        current_link_node,
        overwrite_local,
    )
    .await
}

/// The current revision's registry row of the link `current_link_node` at `at`, and the context
/// of the repository it links, refusing a mount that overlaps one staged since.
async fn restorable_link(
    at: &NodeMapping,
    state_current: &State,
    current_link_node: &Node,
) -> Result<(LinkReference, Arc<RepositoryContext>), LinkError> {
    let link_id = current_link_node.linked_node().repository;
    let current_link_ref = state_current
        .link_find(at.repository.clone(), link_id, at.node)
        .await
        .forward::<LinkError>("Failed to find link registry entry")?;

    let linked_repository = at.repository.to_link_context(link_id).await;
    let source_path = link::pinned_source_path(linked_repository.clone(), current_link_node)
        .await
        .forward::<LinkError>("Failed resolving link source path")?;
    link::check_source_path_overlap(
        &at.state,
        at.repository.clone(),
        linked_repository.clone(),
        source_path,
        at.node,
        None,
    )
    .await
    .forward::<LinkError>("Failed checking link source paths")?;

    Ok((current_link_ref, linked_repository))
}

/// Puts the registry row and the pinned content of `restorable`, from [`restorable_link`], back at
/// the mount `at`, as [`reset_staged_remove_link`] describes. A dry run leaves the files.
async fn restore_link(
    operation: &Arc<InstanceOperationImpl>,
    at: NodeMapping,
    (current_link_ref, linked_repository): (LinkReference, Arc<RepositoryContext>),
    current_link_node: Node,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let NodeMapping {
        repository,
        state: state_staged,
        path: link_path,
        ..
    } = at;

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

    if execution_context().globals().dry_run() {
        return Ok(());
    }
    link::realize_link_pin_change_boxed(
        operation,
        repository,
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
    .await
}

/// Puts a link whose pin, mount or registry row change was staged back to the pin, mount and row
/// the current revision holds, on disk and in the registry.
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

    if staged_pin != current_pin || !staged_link_node.mounts_same_source(&current_link_node) {
        link::realize_link_pin_change_boxed(
            operation,
            repository.clone(),
            repository.to_link_context(link_id).await,
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
    }

    state_staged
        .link_replace(repository, prev_ref)
        .await
        .forward::<LinkError>("Failed to update link registry entry")?;

    Ok(())
}

/// Drops the staged link `staged_link_node` to another repository than the current revision's
/// link `current_link_node` at the mount `at`, and restores the latter. Refuses before it changes
/// anything where the latter's mount overlaps one staged since, or, unless `overwrite_local`, where
/// a sync moving the link back would ([`crate::fs::realize::verify_mount_replacement`]).
pub(crate) async fn reset_staged_relink(
    operation: &Arc<InstanceOperationImpl>,
    at: NodeMapping,
    state_current: Arc<State>,
    staged_link_node: Node,
    current_link_node: Node,
    overwrite_local: bool,
) -> Result<(), LinkError> {
    let restorable = restorable_link(&at, &state_current, &current_link_node).await?;
    if !overwrite_local {
        let (shown, incoming) = tokio::join!(
            open_mounted_tree(&at.repository, &staged_link_node),
            open_mounted_tree(&at.repository, &current_link_node),
        );
        let Some((node, (repository, state))) = incoming else {
            return Err(LinkError::internal(format!(
                "Failed to open the revision the link at {} pins",
                at.path
            )));
        };
        crate::fs::realize::verify_mount_replacement(
            operation,
            &at.repository,
            &at.path,
            shown,
            NodeMapping {
                repository,
                state,
                path: at.path.clone(),
                node,
            },
        )
        .await
        .forward::<LinkError>("Failed verifying the link mount")?;
    }

    // Verified above, or discarded where `overwrite_local`.
    drop_staged_link(operation, &at, &staged_link_node, true).await?;
    restore_link(operation, at, restorable, current_link_node, true).await
}

/// Puts the branch and flags of the current revision's registry row for the link `link_id` at
/// `link_node_id` back on its staged row, and leaves the pin to the changes staged inside the link.
pub(crate) async fn reset_staged_link_branching(
    repository: Arc<RepositoryContext>,
    state_staged: &State,
    state_current: &State,
    link_id: RepositoryId,
    link_node_id: NodeID,
) -> Result<(), LinkError> {
    let (staged, current) = tokio::try_join!(
        state_staged.link_find(repository.clone(), link_id, link_node_id),
        state_current.link_find(repository.clone(), link_id, link_node_id),
    )
    .forward::<LinkError>("Failed to find the link's registry rows")?;
    if staged.branch == current.branch && staged.flags == current.flags {
        return Ok(());
    }

    state_staged
        .link_replace(
            repository,
            LinkReference {
                branch: current.branch,
                flags: current.flags,
                ..staged
            },
        )
        .await
        .forward::<LinkError>("Failed to restore the link's branch and flags")
}
