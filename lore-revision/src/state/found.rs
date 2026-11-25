// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use super::Node;
use super::NodeID;
use super::NodeLink;
use super::NodeMapping;
use super::NodeNotFound;
use super::State;
use super::StateError;
use crate::link::LinkedState;
use crate::repository::RepositoryContext;
use crate::util::path::RelativePath;

/// A node a path lookup found, readable only through the tree that numbers it.
///
/// A lookup that crosses a link finds a node numbered in the linked repository, where the same id
/// names an unrelated node of the tree searched. The id is reached only where the tree searched
/// numbers it ([`Self::own_node`]), together with the pair that numbers it ([`Self::held`]), or as
/// found ([`Self::identity`]).
pub struct FoundNode {
    link: NodeLink,
    held: Option<LinkedState>,
}

impl Default for FoundNode {
    fn default() -> Self {
        Self {
            link: NodeLink::invalid(),
            held: None,
        }
    }
}

impl FoundNode {
    /// `node` of `state`, the tree searched, reached without crossing a link.
    pub(crate) fn own(node: NodeID, repository: &RepositoryContext, state: &State) -> Self {
        Self {
            link: NodeLink {
                node,
                repository: repository.id,
                revision: state.revision(),
            },
            held: None,
        }
    }

    /// This node, held by `linked` unless a link further in holds it.
    pub(super) fn or_held(self, linked: LinkedState) -> Self {
        Self {
            link: self.link,
            held: self.held.or(Some(linked)),
        }
    }

    /// Whether the lookup found a node other than a root.
    pub fn is_valid(&self) -> bool {
        self.link.is_valid()
    }

    /// Whether the lookup found a node, a root included.
    pub fn is_valid_or_root(&self) -> bool {
        self.link.is_valid_or_root()
    }

    /// Whether the lookup crossed a link to reach the node.
    pub fn crossed_link(&self) -> bool {
        self.held.is_some()
    }

    /// The node in the tree searched, `None` where the lookup found none or crossed a link to
    /// reach it.
    pub fn own_node(&self) -> Option<NodeID> {
        (self.held.is_none() && self.is_valid_or_root()).then_some(self.link.node)
    }

    /// [`Self::own_node`], or a not-found error where it is `None`.
    pub fn into_own_node(self) -> Result<NodeID, StateError> {
        self.own_node().ok_or_else(|| NodeNotFound.into())
    }

    /// The node with the repository and state that number it: the linked pair where the lookup
    /// crossed a link, and `searched`, the pair the lookup ran on, otherwise.
    pub fn held(self, searched: LinkedState) -> HeldNode {
        let (repository, state) = self.held.unwrap_or(searched);
        HeldNode {
            node: self.link.node,
            repository,
            state,
        }
    }

    /// The node, repository and revision as found, whichever tree numbers the node.
    pub fn identity(&self) -> NodeLink {
        self.link
    }
}

/// A node with the repository and state that number it.
#[derive(Clone)]
pub struct HeldNode {
    node: NodeID,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
}

impl HeldNode {
    /// The node, numbered in [`Self::state`].
    pub fn id(&self) -> NodeID {
        self.node
    }

    /// The repository holding [`Self::state`].
    pub fn repository(&self) -> &Arc<RepositoryContext> {
        &self.repository
    }

    /// The state that numbers the node.
    pub fn state(&self) -> &Arc<State> {
        &self.state
    }

    /// Reads the node in the state that numbers it.
    pub async fn read(&self) -> Result<Node, StateError> {
        self.state.node(self.repository.clone(), self.node).await
    }

    /// The node as a mapping at `path`.
    pub fn into_mapping(self, path: RelativePath) -> NodeMapping {
        NodeMapping {
            repository: self.repository,
            state: self.state,
            path,
            node: self.node,
        }
    }

    /// The node with the repository and state that number it.
    pub fn into_parts(self) -> (NodeID, LinkedState) {
        (self.node, (self.repository, self.state))
    }
}

impl State {
    /// The node `found` names, read in the tree that numbers it, where this state of `repository`
    /// is the tree the lookup ran on.
    pub(crate) async fn found_node(
        &self,
        repository: Arc<RepositoryContext>,
        found: &FoundNode,
    ) -> Result<Node, StateError> {
        match &found.held {
            Some((repository, state)) => state.node(repository.clone(), found.link.node).await,
            None => self.node(repository, found.link.node).await,
        }
    }
}
