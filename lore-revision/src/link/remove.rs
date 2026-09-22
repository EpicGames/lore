// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::errors::InvalidPath;
use crate::errors::LocalModifications;
use crate::errors::NotALink;
use crate::event;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::FilesystemDiffTree;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
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

    let link_id: RepositoryId = link_node.address.context.into();
    let is_staged_add = link_node.is_staged_add();

    // One operation covers the removal: the working tree read to find local changes is the one
    // the mount is then deleted from.
    with_operation(repository.file_system(), async |operation| {
        if !execution_context().globals().force() {
            verify_no_local_changes_under_link(
                &operation,
                repository.clone(),
                state_current.clone(),
                state_staged.clone(),
                &link_path,
            )
            .await?;
        }

        remove_link_mount(&operation, &link_path, is_staged_add).await
    })
    .await?;

    if is_staged_add {
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
            link_path.clone(),
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

/// Boxed version of [`remove`] for cross-crate use.
pub fn remove_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
) -> crate::BoxFuture<'_, Result<(), LinkError>> {
    Box::pin(remove(repository, token, link_path))
}

/// Removing a link deletes its mounted directory from disk, taking any
/// uncommitted work inside it with no way back.
async fn verify_no_local_changes_under_link(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    link_path: &RelativePath,
) -> Result<(), LinkError> {
    // TODO(vri): narrow this to `link_path` once a scoped diff works.
    // `find_relative_node_link` returns a mount as the parent's own link node,
    // so `diff_filesystem` has nothing to resolve and walks a node with no
    // children, reporting every file under the mount as added.
    let changes = state::diff_filesystem(
        operation,
        FilesystemDiffTree {
            repository: repository.clone(),
            state: state_staged,
        },
        FilesystemDiffTree {
            repository,
            state: state_current,
        },
        None,
        // Not `Full`: removal takes the whole mount, ignored files with it, so
        // they have to count as local changes.
        FilterMode::View,
        FilesystemDiffIntent::Report,
        Arc::new(Vec::new()),
    )
    .await
    .forward::<LinkError>("Failed comparing link content with the file system")?;
    let modified = changes
        .any(|change| link_path.covers_ignore_case(change.path()))
        .await
        .forward::<LinkError>("Failed comparing link content with the file system")?;

    if modified {
        lore_warn!(
            "Link at '{}' has locally modified files (use --force to discard)",
            link_path.as_str()
        );
        return Err(LocalModifications.into());
    }

    Ok(())
}

/// Deletes the directory a link was mounted at, and leaves an empty one behind where the link
/// was only ever staged for add.
///
/// The mount sits at the full link path regardless of nesting, the spelling the operation
/// resolves. A link staged for add replaced whatever the path held, so removing it leaves the
/// path reporting as an unstaged change rather than as a deletion of content the repository
/// never committed.
async fn remove_link_mount(
    operation: &InstanceOperationImpl,
    link_path: &RelativePath,
    is_staged_add: bool,
) -> Result<(), LinkError> {
    operation
        .remove_recursive(link_path)
        .await
        .forward_with::<LinkError, _>(|| format!("Failed to delete directory {link_path}"))?;

    if is_staged_add {
        let _ = operation.create_dir_all(link_path).await;
    }

    Ok(())
}
