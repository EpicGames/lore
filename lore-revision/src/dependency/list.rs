// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::DEPENDENCIES_KEY;
use super::DEPENDENTS_KEY;
use super::DependencyData;
use super::DependencyEntry;
use super::DependencyError;
use super::add::resolve_path;
use super::load_dependency_data;
use super::resolve::transitive_closure;
use crate::lore::execution_context;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;

/// A single file's dependency listing result.
pub struct ListFileResult {
    /// The node ID of the file.
    pub node: NodeID,
    /// The path of the file.
    pub path: String,
    /// Filtered dependency entries for this file.
    pub entries: Vec<ListEntry>,
}

/// A single dependency entry in a listing.
pub struct ListEntry {
    /// Target node ID.
    pub node: NodeID,
    /// Relative path of the target file.
    pub path: String,
    /// Tags on this dependency edge.
    pub tags: Vec<Box<str>>,
    /// BFS depth (0 for direct dependencies).
    pub depth: u32,
}

/// List file dependencies for the specified files.
///
/// If `paths` is empty, returns an empty result (callers should use
/// tree enumeration at a higher level to discover all files with
/// dependency metadata).
///
/// When `reverse` is `true`, queries back-references (dependents)
/// instead of forward dependencies.
///
/// When `recursive` is `true`, performs BFS traversal to collect
/// transitive dependencies. `depth_limit` of `0` means unlimited.
///
/// When `tags` is non-empty, only entries matching at least one of the
/// specified tags are included.
pub async fn list_file_dependencies(
    repository: Arc<RepositoryContext>,
    paths: &[&str],
    recursive: bool,
    reverse: bool,
    tags: &[&str],
    depth_limit: u32,
    revision_specifier: Option<String>,
) -> Result<Vec<ListFileResult>, DependencyError> {
    let signature = if let Some(rev) = revision_specifier {
        revision::resolve(
            repository.clone(),
            rev,
            execution_context().globals().search_location(),
        )
        .await
        .forward::<DependencyError>("resolving revision")?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<DependencyError>("deserializing current anchor")?;
        crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .unwrap_or(current_revision)
    };

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<DependencyError>("deserializing state")?;

    let key = if reverse {
        DEPENDENTS_KEY
    } else {
        DEPENDENCIES_KEY
    };
    let mut results = Vec::new();

    for &path in paths {
        let node = resolve_path(&repository, &state, path).await?;

        let data = load_dependency_data(repository.clone(), &state, node, key).await?;
        let filtered = filter_entries(&data, tags);

        let mut entries: Vec<ListEntry> = Vec::with_capacity(filtered.len());
        for e in &filtered {
            let entry_path = state
                .node_path(repository.clone(), e.node)
                .await
                .forward::<DependencyError>("resolving dependency node path")?;
            entries.push(ListEntry {
                node: e.node,
                path: entry_path,
                tags: e.tags.clone(),
                depth: 0,
            });
        }

        if recursive && depth_limit != 1 {
            let direct_nodes: Vec<NodeID> = entries.iter().map(|e| e.node).collect();
            let transitive = transitive_closure(
                repository.clone(),
                state.clone(),
                &direct_nodes,
                key,
                tags,
                depth_limit.saturating_sub(1),
                false,
            )
            .await?;

            // Add transitive entries not already present as direct deps
            for trans_node in transitive {
                if !entries.iter().any(|e| e.node == trans_node) && trans_node != node {
                    let trans_data =
                        load_dependency_data(repository.clone(), &state, trans_node, key).await?;
                    let trans_filtered = filter_entries(&trans_data, tags);
                    let depth = 1; // Simplified: mark all transitive entries as depth 1+
                    for _entry in &trans_filtered {
                        // The transitive node itself is the entry
                    }
                    let trans_path = state
                        .node_path(repository.clone(), trans_node)
                        .await
                        .forward::<DependencyError>(
                        "resolving transitive dependency node path",
                    )?;
                    entries.push(ListEntry {
                        node: trans_node,
                        path: trans_path,
                        tags: vec![],
                        depth,
                    });
                }
            }
        }

        results.push(ListFileResult {
            node,
            path: path.to_string(),
            entries,
        });
    }

    Ok(results)
}

fn filter_entries<'a>(data: &'a DependencyData, tags: &[&str]) -> Vec<&'a DependencyEntry> {
    data.iter().filter(|e| e.matches_tags(tags)).collect()
}
