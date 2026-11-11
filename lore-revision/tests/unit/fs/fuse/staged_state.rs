// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a staged State records of a mount's writes, read two ways: the dirty actions its nodes
//! carry, and what `status` reports from it.

use std::collections::BTreeMap;
use std::sync::Arc;

use lore_revision::interface::LoreFileAction;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

/// The dirty action a node carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add,
    Modify,
    Delete,
}

/// Every node of `state` carrying a dirty action, by path, with its extension flags.
pub async fn dirty_actions(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
) -> BTreeMap<String, (Action, NodeExtFlags)> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((node_id, path)) = pending.pop() {
        let children = state
            .node_children(repository.clone(), node_id)
            .await
            .expect("children of a staged directory");
        for child_id in children {
            let child = state
                .node(repository.clone(), child_id)
                .await
                .expect("a staged node");
            if !child.is_dirty() {
                continue;
            }
            let name = state
                .node_name_clone(repository.clone(), child_id)
                .await
                .expect("a staged node's name");
            let child_path = if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            };
            let action = if child.is_dirty_delete() {
                Some(Action::Delete)
            } else if child.is_dirty_add() {
                Some(Action::Add)
            } else if child.is_dirty_modify() {
                Some(Action::Modify)
            } else {
                None
            };
            if let Some(action) = action {
                found.insert(child_path.clone(), (action, child.ext_flags()));
            }
            if child.is_directory() {
                pending.push((child_id, child_path));
            }
        }
    }
    found
}

/// What `status` reports of `repository`, by path: the action and whether the change is dirty.
/// `scan` asks for `--scan`.
pub async fn status_files(
    repository: Arc<RepositoryContext>,
    scan: bool,
) -> BTreeMap<String, (LoreFileAction, bool)> {
    crate::helper::test_status_files_at(repository, None, scan).await
}
