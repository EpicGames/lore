// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod audit;
pub mod diff;
pub mod dirty;
pub mod dump;
pub mod hash;
pub mod history;
pub mod info;
pub mod obliterate;
pub mod reset;
pub mod stage;
pub mod unstage;
pub mod write;

use std::sync::Arc;

use crate::errors::InvalidArguments;
use crate::errors::NodeNotFound;
use crate::errors::NotSupported;
use crate::errors::StateErrors;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;

/// The node at `path`, refused as not supported where `path` is in a link, whose node belongs to
/// the linked repository's tree rather than this one, or names a link's mount, and
/// `NodeNotFound` where the tree holds nothing at `path`.
pub(crate) async fn node_outside_links(
    state: &State,
    repository: &Arc<RepositoryContext>,
    path: &str,
) -> Result<NodeID, StateError> {
    let node_link = state.find_node_link(repository.clone(), path).await?;
    if node_link.crosses_link(repository, state)
        || state
            .node(repository.clone(), node_link.node)
            .await?
            .is_link()
    {
        return Err(NotSupported {
            operation: format!(
                "Moving or copying a path into, out of or within a link, or a link's mount, at {path}"
            ),
        }
        .into());
    }
    Ok(node_link.node)
}

/// The node at `path`, or `None` where the tree holds none, refused as [`node_outside_links`]
/// refuses for `path` or else for its nearest ancestor the tree holds.
pub(crate) async fn existing_node_outside_links(
    state: &State,
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
) -> Result<Option<NodeID>, StateError> {
    match node_outside_links(state, repository, path.as_str()).await {
        Err(StateErrors::NodeNotFound(_)) => {}
        found => return found.map(Some),
    }
    let mut ancestor = path.parent_path();
    while !ancestor.is_empty() {
        match node_outside_links(state, repository, ancestor.as_str()).await {
            Err(StateErrors::NodeNotFound(_)) => ancestor.pop_name(),
            found => return found.map(|_| None),
        }
    }
    Ok(None)
}

/// The node of the directory holding `path`, refused as [`existing_node_outside_links`] refuses:
/// `NodeNotFound` where the tree holds no such directory, and `InvalidArguments` for the root,
/// which no directory holds.
pub(crate) async fn parent_outside_links(
    state: &State,
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
) -> Result<NodeID, StateError> {
    if path.is_empty() {
        return Err(InvalidArguments {
            reason: "The repository root has no parent directory".into(),
        }
        .into());
    }
    let parent = path.parent_path();
    if parent.is_empty() {
        return Ok(ROOT_NODE);
    }
    existing_node_outside_links(state, repository, &parent)
        .await?
        .ok_or_else(|| NodeNotFound.into())
}
