// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LinkError;
use crate::branch;
use crate::errors::InvalidPath;
use crate::event;
use crate::filter::FilterMode;
use crate::interface::LoreFileAction;
use crate::link;
use crate::link::LinkFlags;
use crate::link::LoreLinkChangeEventData;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::node::Node;
use crate::node::NodeFlags;
use crate::node::ROOT_NODE;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::clone;
use crate::repository::clone::CloneStats;
use crate::repository::clone::LoreRepositoryCloneBeginEventData;
use crate::repository::clone::LoreRepositoryCloneCountData;
use crate::repository::clone::LoreRepositoryCloneEndEventData;
use crate::stage;
use crate::stage::StageOptions;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

pub async fn add(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
    link_identifier: String,
    source_path: RelativePath,
    pin: Option<String>,
    disable_branching: bool,
) -> Result<(), LinkError> {
    let (remote_url, name) = repository::parse_url(&link_identifier, false)
        .forward_with::<LinkError, _>(|| {
            format!("Invalid repository URL or ID: {link_identifier}")
        })?;

    let context = execution_context();
    let identity = context.globals().identity().unwrap_or_default();
    let repository_data = repository::resolve_by_name(&remote_url, &name, identity)
        .await
        .forward_with::<LinkError, _>(|| format!("Repository not found: {link_identifier}"))?;

    let link = repository_data.id;

    if link == repository.id {
        return Err(LinkError::internal(
            "Invalid link, a link cannot link to itself",
        ));
    }

    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    lore_debug!("Resolve link {link} {source_path}");
    let link = Arc::new(repository.to_link_context(link).await);

    let link_remote = link.remote().await.forward::<LinkError>("Not connected")?;

    // Determine the link branch and revision based on --pin and --disable-branching
    let (link_revision, link_branch) = if disable_branching {
        if let Some(pin) = pin {
            link::resolve_pin(link.clone(), pin).await?
        } else {
            // Use the linked repo's default branch latest
            let link_metadata = repository::metadata_hash(link.clone())
                .await
                .forward::<LinkError>("Failed to load repository metadata")?;
            let link_metadata = repository::metadata(link.clone(), link_metadata)
                .await
                .forward::<LinkError>("Failed to load repository metadata")?;
            let default_branch_id = link_metadata.default_branch;

            let link_latest =
                branch::load_remote_latest(link_remote.clone(), link.id, default_branch_id)
                    .await
                    .forward::<LinkError>("Failed to load link latest")?;

            lore_debug!("Using default branch {default_branch_id} at LATEST ({link_latest})");

            (link_latest, default_branch_id)
        }
    } else {
        // Branching enabled: ensure a matching branch exists in the linked repo
        let current_branch_id = current_branch;

        let branch_latest = if let Ok(link_latest) =
            branch::load_remote_latest(link_remote.clone(), link.id, current_branch_id).await
        {
            lore_debug!("Using existing link branch at LATEST ({link_latest})");
            link_latest
        } else {
            let link_metadata = repository::metadata_hash(link.clone())
                .await
                .forward::<LinkError>("Failed to load repository metadata")?;
            let link_metadata = repository::metadata(link.clone(), link_metadata)
                .await
                .forward::<LinkError>("Failed to load repository metadata")?;
            let default_branch_id = link_metadata.default_branch;

            let branch_metadata = branch::metadata(repository.clone(), current_branch_id)
                .await
                .forward::<LinkError>("Failed getting branch metadata")?;
            let branch_name = branch::name(&branch_metadata)
                .forward::<LinkError>("Failed getting branch metadata")?;
            let branch_category = branch::category(&branch_metadata).unwrap_or_default();

            let parent_latest =
                branch::load_remote_latest(link_remote.clone(), link.id, default_branch_id)
                    .await
                    .forward::<LinkError>("Failed getting branch metadata")?;

            let link_latest = link::create_branch(
                link.clone(),
                link_remote.clone(),
                current_branch_id,
                branch_name.into(),
                branch_category.into(),
                default_branch_id,
                parent_latest,
            )
            .await?;

            lore_debug!(
                "Created branch {} at LATEST ({link_latest}) in linked repo",
                current_branch_id
            );

            link_latest
        };

        let link_revision = if let Some(pin) = pin {
            let (pin_revision, _pin_branch) = link::resolve_pin(link.clone(), pin).await?;
            lore_debug!("Using pinned revision {pin_revision} on branch {current_branch_id}");
            pin_revision
        } else {
            branch_latest
        };

        (link_revision, current_branch_id)
    };

    let branch_metadata = branch::metadata(link.clone(), link_branch)
        .await
        .forward::<LinkError>("Failed getting branch metadata")?;
    let branch_name =
        branch::name(&branch_metadata).forward::<LinkError>("Failed getting branch metadata")?;

    lore_debug!("Load link revision state");
    let link_state = State::deserialize(link.clone(), link_revision)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    lore_debug!("Find link target node for {source_path}");
    let link_node_link = link_state
        .find_node_link(link.clone(), source_path.as_str())
        .await
        .forward::<LinkError>("Invalid path")?;

    lore_debug!("Link target node is {link_node_link:?}");
    if !link_node_link.is_valid_or_root() {
        return Err(InvalidPath {
            path: source_path.to_string(),
        }
        .into());
    }

    // Target node must be in the given link repository, not a link itself
    if link_node_link.repository != link.id {
        return Err(LinkError::internal(
            "Link path is a link itself, link to the target repository directly",
        ));
    }

    // Target node must be a directory
    let link_node = link_state
        .node(link.clone(), link_node_link.node)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    if !link_node.is_directory() {
        return Err(LinkError::internal(
            "Link path must be a directory in the target repository",
        ));
    }

    let absolute_path = link_path.to_absolute_path(repository.require_path()?);

    // If a directory already exists, make sure it doesn't have any children
    let link_path_exists = match lore_io::IoDriver::global()
        .read_dir(absolute_path.as_path())
        .await
    {
        Ok(mut entries) => {
            if entries
                .next()
                .await
                .transpose()
                .internal("Failed to check link path")?
                .is_some()
            {
                return Err(LinkError::internal(format!(
                    "Link path already has children {}",
                    absolute_path.display()
                )));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(LinkError::internal(format!(
                "Failed to check link path: {error}"
            )));
        }
    };

    if let Ok(node_link) = state_staged
        .find_node_link(repository.clone(), link_path.as_str())
        .await
        && let Ok(node) = state_staged.node(repository.clone(), node_link.node).await
        // Allow re-adding a link to a path that is staged for delete
        && !node.is_staged_delete()
    {
        // Prevent adding a link to a link
        // TODO(vri): UCS-17744 - Allow adding nested links
        if node_link.repository != repository.id {
            return Err(LinkError::internal(
                "Link path cannot be in a linked repository: Nested link",
            ));
        }

        // Prevent linking into file or other link
        if !node.is_directory() {
            return Err(LinkError::internal(format!(
                "Link path is already a link {}",
                absolute_path.display()
            )));
        }

        let mut children = StateNodeChildrenIterator::new(
            state_staged.clone(),
            repository.clone(),
            node_link.node,
        )
        .await
        .forward::<LinkError>("Failed deserializing state node block")?;

        // Prevent the directory having children
        if let Ok(child) = children.next().await
            && child.is_some()
        {
            return Err(LinkError::internal(format!(
                "Link path already has children {}",
                absolute_path.display()
            )));
        }
    };

    let mut parent_path = link_path.clone();
    parent_path.pop();

    if !parent_path.is_empty() {
        let parent_absolute_path = parent_path.to_absolute_path(repository.require_path()?);

        if lore_io::IoDriver::global()
            .metadata(parent_absolute_path.as_path())
            .await
            .is_err()
        {
            lore_debug!("Creating directory {parent_absolute_path:?}");
            lore_io::IoDriver::global()
                .create_dir_all(parent_absolute_path.as_path())
                .await
                .internal_with(|| {
                    format!(
                        "Failed to create directory {}",
                        parent_absolute_path.display()
                    )
                })?;
        }

        lore_debug!("Staging link parent path");
        Box::pin(stage::stage_filesystem_path(
            repository.clone(),
            state_staged.clone(),
            repository.require_path()?.to_path_buf(),
            RelativePathBuf::new(),
            ROOT_NODE,
            parent_path,
            Arc::default(),
            StageOptions {
                no_children: true,
                ..Default::default()
            },
            None, // No link tracking when adding links
            None, // No layer mask
        ))
        .await
        .forward::<LinkError>("Failed staging the link node")?;
    }

    if !link_path_exists {
        lore_debug!("Creating directory {link_path}");
        lore_io::IoDriver::global()
            .create_dir_all(absolute_path.as_path())
            .await
            .internal_with(|| format!("Failed to create directory {}", absolute_path.display()))?;
    }

    lore_debug!("Staging link node");
    let node = Node {
        flags: NodeFlags::Link.bits(),
        child: link_node_link.node,
        address: Address {
            hash: link_revision,
            context: link.id.into(),
        },
        ..Default::default()
    };
    let link_node = stage::stage_single_node(
        repository.clone(),
        state_staged.clone(),
        link_path.clone(),
        node,
        Arc::default(),
        None, // No link tracking when adding links
        FilterMode::Full,
    )
    .await
    .forward::<LinkError>("Failed staging the link node")?;

    let (link_flags, stored_branch) = if disable_branching {
        lore_debug!("Disabled auto-follow for link {}", link.id);
        (LinkFlags::DisableAutoFollow, link_branch)
    } else {
        (LinkFlags::NoFlags, BranchId::default())
    };

    state_staged
        .link_add(
            repository.clone(),
            link.id,
            stored_branch,
            link_revision,
            link_node.node,
            link_flags,
        )
        .await
        .forward::<LinkError>("Failed to add link")?;

    // Clone the link in the path
    lore_debug!("Connecting remote storage");
    let correlation_id = execution_context().globals().correlation_id.to_string();
    let storage = link_remote
        .session(link.id, &correlation_id)
        .await
        .forward::<LinkError>("Not connected")?;

    lore_debug!("Clone link in {link_path}");

    event::LoreEvent::RepositoryCloneBegin(LoreRepositoryCloneBeginEventData {
        repository: link.id,
        branch: branch_name.into(),
        revision: link_state.revision(),
        path: repository.require_path()?.into(),
    })
    .send();

    let stats = Arc::new(CloneStats::default());
    clone::clone_node(
        link.clone(),
        storage,
        link_state,
        absolute_path,
        source_path,
        link_node_link.node,
        Arc::default(), /* Default options */
        stats.clone(),
    )
    .await
    .forward::<LinkError>("Failed cloning target link")?;

    event::LoreEvent::RepositoryCloneEnd(LoreRepositoryCloneEndEventData {
        branch: branch_name.into(),
        revision: link_revision,
        count: LoreRepositoryCloneCountData::new(&stats),
    })
    .send();

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
        LoreFileAction::Add,
    ))
    .send();

    Ok(())
}
