// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::DEPENDENCIES_KEY;
use super::DEPENDENTS_KEY;
use super::DependencyError;
use super::add::flush_state;
use super::add::resolve_path;
use super::load_dependency_data;
use super::store_dependency_data;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;

/// Result of a remove operation.
pub struct RemoveResult {
    /// Number of dependency edges fully removed.
    pub removed: usize,
}

/// Remove file dependencies for one or more source files.
///
/// Each entry in `sources` is a `(source_path, dependencies)` pair where
/// `dependencies` is a slice of `(dependency_path, tags)`. If `tags` is
/// empty for a dependency, the entire dependency edge is removed. If tags
/// are specified, only those tags are removed and the edge is removed
/// entirely when no tags remain.
///
/// Corresponding back-references on target files are updated
/// automatically.
pub async fn remove_file_dependencies(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    sources: &[super::add::SourceSpec<'_>],
) -> Result<RemoveResult, DependencyError> {
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

    let mut removed = 0usize;

    // Track back-reference removals: target_node -> [(source_node, tags)]
    let mut backref_removals: HashMap<NodeID, Vec<(NodeID, Vec<&str>)>> = HashMap::new();

    for &(source_path, deps) in sources {
        let source_node = resolve_path(&repository, &state, source_path).await?;

        let mut forward_data =
            load_dependency_data(repository.clone(), &state, source_node, DEPENDENCIES_KEY).await?;

        for &(dep_path, tags) in deps {
            let dep_node = resolve_path(&repository, &state, dep_path).await?;

            if forward_data.remove(dep_node, tags) {
                removed += 1;
            }

            backref_removals
                .entry(dep_node)
                .or_default()
                .push((source_node, tags.to_vec()));
        }

        store_dependency_data(
            repository.clone(),
            &state,
            source_node,
            DEPENDENCIES_KEY,
            &forward_data,
        )
        .await?;
    }

    // Remove back-references
    for (target_node, removals) in &backref_removals {
        let mut backref_data =
            load_dependency_data(repository.clone(), &state, *target_node, DEPENDENTS_KEY).await?;

        for (source_node, tags) in removals {
            let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_ref()).collect();
            backref_data.remove(*source_node, &tag_refs);
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

    flush_state(
        &repository,
        token,
        &state,
        current_revision,
        staged_revision,
    )
    .await?;

    Ok(RemoveResult { removed })
}
