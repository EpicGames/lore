// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::DEPENDENCIES_KEY;
use super::DEPENDENTS_KEY;
use super::DependencyError;
use super::load_dependency_data;
use super::resolve;
use super::store_dependency_data;
use crate::errors::FileNotFound;
use crate::lore::Hash;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

/// A `(dependency_path, tags)` pair.
pub type DepSpec<'a> = (&'a str, &'a [&'a str]);
/// A `(source_path, dependencies)` pair for bulk operations.
pub type SourceSpec<'a> = (&'a str, &'a [DepSpec<'a>]);

/// Result of an add operation.
pub struct AddResult {
    /// Number of new dependency edges added.
    pub added: usize,
}

/// Add file dependencies for one or more source files.
///
/// Each entry in `sources` is a `(source_path, dependencies)` pair where
/// `dependencies` is a slice of `(dependency_path, tags)`. For each source
/// file, the forward dependencies are updated and corresponding
/// back-references are maintained on target files.
///
/// When `force` is `true`, cycle detection is skipped.
pub async fn add_file_dependencies(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    sources: &[SourceSpec<'_>],
    force: bool,
) -> Result<AddResult, DependencyError> {
    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<DependencyError>("deserializing current anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<DependencyError>("deserializing state")?;

    // Resolve all paths to NodeIDs upfront
    type ResolvedDep<'a> = (NodeID, Vec<&'a str>);
    let mut resolved_sources: Vec<(NodeID, Vec<ResolvedDep<'_>>)> = Vec::new();

    for &(source_path, deps) in sources {
        let source_node = resolve_path(&repository, &state, source_path).await?;

        let mut resolved_deps = Vec::new();
        for &(dep_path, tags) in deps {
            let dep_node = resolve_path(&repository, &state, dep_path).await?;
            resolved_deps.push((dep_node, tags.to_vec()));
        }

        resolved_sources.push((source_node, resolved_deps));
    }

    // Cycle detection (unless forced)
    if !force {
        for (source_node, deps) in &resolved_sources {
            let target_nodes: Vec<NodeID> = deps.iter().map(|(node, _)| *node).collect();
            resolve::check_cycle(
                repository.clone(),
                state.clone(),
                *source_node,
                &target_nodes,
                DEPENDENCIES_KEY,
            )
            .await?;
        }
    }

    // Collect all forward and backward modifications
    let mut added = 0usize;

    // Track back-reference updates: target_node -> [(source_node, tags)]
    let mut backref_updates: HashMap<NodeID, Vec<(NodeID, Vec<&str>)>> = HashMap::new();

    // Apply forward dependencies
    for (source_node, deps) in &resolved_sources {
        let mut forward_data =
            load_dependency_data(repository.clone(), &state, *source_node, DEPENDENCIES_KEY)
                .await?;

        for (dep_node, tags) in deps {
            let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_ref()).collect();
            let had_entry = forward_data.contains(*dep_node);
            forward_data.add(*dep_node, &tag_refs);
            if !had_entry {
                added += 1;
            }

            backref_updates
                .entry(*dep_node)
                .or_default()
                .push((*source_node, tags.clone()));
        }

        store_dependency_data(
            repository.clone(),
            &state,
            *source_node,
            DEPENDENCIES_KEY,
            &forward_data,
        )
        .await?;
    }

    // Apply back-references
    for (target_node, updates) in &backref_updates {
        let mut backref_data =
            load_dependency_data(repository.clone(), &state, *target_node, DEPENDENTS_KEY).await?;

        for (source_node, tags) in updates {
            let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_ref()).collect();
            backref_data.add(*source_node, &tag_refs);
        }

        store_dependency_data(
            repository.clone(),
            &state,
            *target_node,
            DEPENDENTS_KEY,
            &backref_data,
        )
        .await?;
    }

    // Serialize state if modified
    flush_state(
        &repository,
        token,
        &state,
        current_revision,
        staged_revision,
    )
    .await?;

    Ok(AddResult { added })
}

pub async fn resolve_path(
    repository: &Arc<RepositoryContext>,
    state: &State,
    path: &str,
) -> Result<NodeID, DependencyError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path)
        .map_err(|_e| FileNotFound {
            resource: path.to_string(),
        })?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_err(|_e| FileNotFound {
            resource: path.to_string(),
        })?;

    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: path.to_string(),
        }
        .into());
    }

    Ok(node_link.node)
}

pub(super) async fn flush_state(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: &State,
    current_revision: Hash,
    staged_revision: Hash,
) -> Result<(), DependencyError> {
    if state.is_dirty() {
        state.set_revision_number(0);
        state.set_parent_self(current_revision);

        if staged_revision == current_revision {
            state.set_parent_other(Hash::default());
        }

        let signature = state
            .serialize(repository.clone(), token)
            .await
            .forward::<DependencyError>("serializing state")?;

        crate::instance::store_staged_anchor(repository, signature)
            .await
            .forward::<DependencyError>("flushing stores and serializing staged anchor")?;
    }

    Ok(())
}
