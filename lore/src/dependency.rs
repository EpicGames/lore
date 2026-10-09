// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_macro::LoreArgs;
use lore_revision::dependency;
use lore_revision::dependency::DependencyError;
use lore_revision::dependency::LoreFileDependencyAddBeginEventData;
use lore_revision::dependency::LoreFileDependencyAddEndEventData;
use lore_revision::dependency::LoreFileDependencyAddEntryEventData;
use lore_revision::dependency::LoreFileDependencyListBeginEventData;
use lore_revision::dependency::LoreFileDependencyListEndEventData;
use lore_revision::dependency::LoreFileDependencyListEntryEventData;
use lore_revision::dependency::LoreFileDependencyListFileEndEventData;
use lore_revision::dependency::LoreFileDependencyListFileEventData;
use lore_revision::dependency::LoreFileDependencyRemoveBeginEventData;
use lore_revision::dependency::LoreFileDependencyRemoveEndEventData;
use lore_revision::dependency::LoreFileDependencyRemoveEntryEventData;
use lore_revision::dependency::add::SourceSpec;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEvent;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;

use crate::call::repository_call_read;
use crate::call::repository_call_write;
use crate::call_delegation::dispatch_call;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(dependency_add_local)]
/// Arguments for adding file dependencies, expanded from flat parallel arrays.
pub struct LoreFileDependencyAddArgs {
    /// Source file paths that will have dependencies added.
    pub paths: LoreArray<LoreString>,
    /// Dependency target file paths (flat array).
    pub dependencies: LoreArray<LoreString>,
    /// Tags to apply to the added dependencies (flat array).
    pub tags: LoreArray<LoreString>,
    /// Number of dependencies per source file path.
    pub dep_counts: LoreArray<u32>,
    /// Number of tags per dependency entry.
    pub tag_counts: LoreArray<u32>,
    /// Skip cycle detection.
    pub force: u8,
}

pub async fn dependency_add(
    globals: LoreGlobalArgs,
    args: LoreFileDependencyAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dependency_add_local).await
}

fn dependency_add_local(
    globals: LoreGlobalArgs,
    args: LoreFileDependencyAddArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    repository_call_write(
        globals,
        callback,
        args,
        dependency_add,
        |repository, token, args| async move { dependency_add_impl(repository, &token, args).await },
    )
}

async fn dependency_add_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreFileDependencyAddArgs,
) -> Result<(), DependencyError> {
    let sources = expand_source_specs(
        args.paths.as_slice(),
        args.dependencies.as_slice(),
        args.tags.as_slice(),
        args.dep_counts.as_slice(),
        args.tag_counts.as_slice(),
    )?;

    let total_deps: usize = sources.iter().map(|(_, deps)| deps.len()).sum();

    LoreEvent::FileDependencyAddBegin(LoreFileDependencyAddBeginEventData {
        path_count: sources.len() as u64,
        dependency_count: total_deps as u64,
    })
    .send();

    for (path, deps) in &sources {
        for (dep, tags) in deps {
            let tag_strings: Vec<LoreString> =
                tags.iter().map(|t| LoreString::from(t.as_str())).collect();
            LoreEvent::FileDependencyAddEntry(LoreFileDependencyAddEntryEventData {
                path: LoreString::from(path.as_str()),
                dependency: LoreString::from(dep.as_str()),
                tags: LoreArray::from_vec(tag_strings),
            })
            .send();
        }
    }

    let force = args.force != 0;

    // Build SourceSpec references with stable intermediate storage.
    // Each layer must outlive the next.
    let tag_vecs: Vec<Vec<Vec<&str>>> = sources
        .iter()
        .map(|(_, deps)| {
            deps.iter()
                .map(|(_, tags)| tags.iter().map(|t| t.as_str()).collect())
                .collect()
        })
        .collect();
    let dep_spec_vecs: Vec<Vec<(&str, &[&str])>> = sources
        .iter()
        .zip(tag_vecs.iter())
        .map(|((_, deps), tvecs)| {
            deps.iter()
                .zip(tvecs.iter())
                .map(|((dep, _), tv)| (dep.as_str(), tv.as_slice()))
                .collect()
        })
        .collect();
    let source_specs: Vec<SourceSpec<'_>> = sources
        .iter()
        .zip(dep_spec_vecs.iter())
        .map(|((path, _), dspecs)| (path.as_str(), dspecs.as_slice()))
        .collect();

    let result =
        dependency::add::add_file_dependencies(repository, token, &source_specs, force).await?;

    LoreEvent::FileDependencyAddEnd(LoreFileDependencyAddEndEventData {
        added_count: result.added as u64,
    })
    .send();

    Ok(())
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(dependency_remove_local)]
/// Arguments for removing file dependencies, expanded from flat parallel arrays.
pub struct LoreFileDependencyRemoveArgs {
    /// Source file paths to remove dependencies from.
    pub paths: LoreArray<LoreString>,
    /// Dependency target paths to remove (flat array).
    pub dependencies: LoreArray<LoreString>,
    /// Tags to remove.
    pub tags: LoreArray<LoreString>,
    /// Number of dependencies per source file.
    pub dep_counts: LoreArray<u32>,
    /// Number of tags per dependency entry.
    pub tag_counts: LoreArray<u32>,
}

pub async fn dependency_remove(
    globals: LoreGlobalArgs,
    args: LoreFileDependencyRemoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dependency_remove_local).await
}

fn dependency_remove_local(
    globals: LoreGlobalArgs,
    args: LoreFileDependencyRemoveArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    repository_call_write(
        globals,
        callback,
        args,
        dependency_remove,
        |repository, token, args| async move { dependency_remove_impl(repository, &token, args).await },
    )
}

async fn dependency_remove_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    args: LoreFileDependencyRemoveArgs,
) -> Result<(), DependencyError> {
    let sources = expand_source_specs(
        args.paths.as_slice(),
        args.dependencies.as_slice(),
        args.tags.as_slice(),
        args.dep_counts.as_slice(),
        args.tag_counts.as_slice(),
    )?;

    let total_deps: usize = sources.iter().map(|(_, deps)| deps.len()).sum();

    LoreEvent::FileDependencyRemoveBegin(LoreFileDependencyRemoveBeginEventData {
        path_count: sources.len() as u64,
        dependency_count: total_deps as u64,
    })
    .send();

    for (path, deps) in &sources {
        for (dep, tags) in deps {
            let tag_strings: Vec<LoreString> =
                tags.iter().map(|t| LoreString::from(t.as_str())).collect();
            LoreEvent::FileDependencyRemoveEntry(LoreFileDependencyRemoveEntryEventData {
                path: LoreString::from(path.as_str()),
                dependency: LoreString::from(dep.as_str()),
                tags: LoreArray::from_vec(tag_strings),
            })
            .send();
        }
    }

    let tag_vecs: Vec<Vec<Vec<&str>>> = sources
        .iter()
        .map(|(_, deps)| {
            deps.iter()
                .map(|(_, tags)| tags.iter().map(|t| t.as_str()).collect())
                .collect()
        })
        .collect();
    let dep_spec_vecs: Vec<Vec<(&str, &[&str])>> = sources
        .iter()
        .zip(tag_vecs.iter())
        .map(|((_, deps), tvecs)| {
            deps.iter()
                .zip(tvecs.iter())
                .map(|((dep, _), tv)| (dep.as_str(), tv.as_slice()))
                .collect()
        })
        .collect();
    let source_specs: Vec<SourceSpec<'_>> = sources
        .iter()
        .zip(dep_spec_vecs.iter())
        .map(|((path, _), dspecs)| (path.as_str(), dspecs.as_slice()))
        .collect();

    let result =
        dependency::remove::remove_file_dependencies(repository, token, &source_specs).await?;

    LoreEvent::FileDependencyRemoveEnd(LoreFileDependencyRemoveEndEventData {
        removed_count: result.removed as u64,
    })
    .send();

    Ok(())
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(dependency_list_local)]
/// Arguments for listing file dependencies (or dependents) at a given revision.
pub struct LoreFileDependencyListArgs {
    /// Files to query.
    pub paths: LoreArray<LoreString>,
    /// Revision to query at.
    pub revision: LoreString,
    /// Follow transitive dependencies recursively.
    pub recursive: u8,
    /// Return dependents instead of dependencies.
    pub reverse: u8,
    /// Filter results by tags.
    pub tags: LoreArray<LoreString>,
    /// Maximum recursion depth (0 = unlimited).
    pub depth_limit: u32,
}

pub async fn dependency_list(
    globals: LoreGlobalArgs,
    args: LoreFileDependencyListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, dependency_list_local).await
}

fn dependency_list_local(
    globals: LoreGlobalArgs,
    args: LoreFileDependencyListArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    repository_call_read(
        globals,
        callback,
        args,
        dependency_list,
        dependency_list_impl,
    )
}

async fn dependency_list_impl(
    repository: Arc<RepositoryContext>,
    args: LoreFileDependencyListArgs,
) -> Result<(), DependencyError> {
    let paths: Vec<&str> = args.paths.as_slice().iter().map(|s| s.as_str()).collect();
    let tags: Vec<&str> = args.tags.as_slice().iter().map(|s| s.as_str()).collect();
    let recursive = args.recursive != 0;
    let reverse = args.reverse != 0;

    let revision: Option<String> = args.revision.into();

    let results = dependency::list::list_file_dependencies(
        repository,
        &paths,
        recursive,
        reverse,
        &tags,
        args.depth_limit,
        revision,
    )
    .await?;

    LoreEvent::FileDependencyListBegin(LoreFileDependencyListBeginEventData {
        file_count: results.len() as u64,
    })
    .send();

    let mut total_entries = 0u64;

    for file_result in &results {
        LoreEvent::FileDependencyListFile(LoreFileDependencyListFileEventData {
            path: LoreString::from(file_result.path.as_str()),
            entry_count: file_result.entries.len() as u64,
        })
        .send();

        for entry in &file_result.entries {
            let tag_strings: Vec<LoreString> = entry
                .tags
                .iter()
                .map(|t| LoreString::from(t.as_ref()))
                .collect();
            LoreEvent::FileDependencyListEntry(LoreFileDependencyListEntryEventData {
                path: LoreString::from(entry.path.as_str()),
                tags: LoreArray::from_vec(tag_strings),
                depth: entry.depth,
            })
            .send();
            total_entries += 1;
        }

        LoreEvent::FileDependencyListFileEnd(LoreFileDependencyListFileEndEventData {
            path: LoreString::from(file_result.path.as_str()),
        })
        .send();
    }

    LoreEvent::FileDependencyListEnd(LoreFileDependencyListEndEventData {
        total_entry_count: total_entries,
    })
    .send();

    Ok(())
}

/// Owned intermediate: `(path, [(dep_path, [tags])])`.
type ExpandedSources = Vec<(String, Vec<(String, Vec<String>)>)>;

/// Expand flat C API arrays into structured per-source dependency lists.
///
/// Modes (from spec):
/// - `dep_counts` empty, `tag_counts` empty: cross-product
/// - `dep_counts` populated, `tag_counts` empty: deps distributed per path, all tags uniform
/// - `dep_counts` populated, `tag_counts` populated: full control
/// - `dep_counts` empty, `tag_counts` populated: invalid
fn expand_source_specs(
    paths: &[LoreString],
    dependencies: &[LoreString],
    tags: &[LoreString],
    dep_counts: &[u32],
    tag_counts: &[u32],
) -> Result<ExpandedSources, DependencyError> {
    if dep_counts.is_empty() && !tag_counts.is_empty() {
        return Err(InvalidArguments {
            reason: "tag_counts cannot be populated when dep_counts is empty".to_string(),
        }
        .into());
    }

    let all_tags: Vec<String> = tags.iter().map(|t| t.as_str().to_string()).collect();

    if dep_counts.is_empty() {
        let mut result = Vec::with_capacity(paths.len());
        for path in paths {
            let deps: Vec<(String, Vec<String>)> = dependencies
                .iter()
                .map(|d| (d.as_str().to_string(), all_tags.clone()))
                .collect();
            result.push((path.as_str().to_string(), deps));
        }
        return Ok(result);
    }

    if dep_counts.len() != paths.len() {
        return Err(InvalidArguments {
            reason: format!(
                "dep_counts length ({}) must match paths length ({})",
                dep_counts.len(),
                paths.len()
            ),
        }
        .into());
    }

    let dep_count_sum: u32 = dep_counts.iter().sum();
    if dep_count_sum as usize != dependencies.len() {
        return Err(InvalidArguments {
            reason: format!(
                "dep_counts sum ({}) must match dependencies length ({})",
                dep_count_sum,
                dependencies.len()
            ),
        }
        .into());
    }

    if tag_counts.is_empty() {
        let mut result = Vec::with_capacity(paths.len());
        let mut dep_offset = 0usize;
        for (i, path) in paths.iter().enumerate() {
            let count = dep_counts[i] as usize;
            let deps: Vec<(String, Vec<String>)> = dependencies[dep_offset..dep_offset + count]
                .iter()
                .map(|d| (d.as_str().to_string(), all_tags.clone()))
                .collect();
            dep_offset += count;
            result.push((path.as_str().to_string(), deps));
        }
        return Ok(result);
    }

    if tag_counts.len() != dependencies.len() {
        return Err(InvalidArguments {
            reason: format!(
                "tag_counts length ({}) must match dependencies length ({})",
                tag_counts.len(),
                dependencies.len()
            ),
        }
        .into());
    }

    let tag_count_sum: u32 = tag_counts.iter().sum();
    if tag_count_sum as usize != tags.len() {
        return Err(InvalidArguments {
            reason: format!(
                "tag_counts sum ({}) must match tags length ({})",
                tag_count_sum,
                tags.len()
            ),
        }
        .into());
    }

    let mut result = Vec::with_capacity(paths.len());
    let mut dep_offset = 0usize;
    let mut tag_offset = 0usize;
    for (i, path) in paths.iter().enumerate() {
        let dep_count = dep_counts[i] as usize;
        let mut deps = Vec::with_capacity(dep_count);
        for j in 0..dep_count {
            let dep_idx = dep_offset + j;
            let tc = tag_counts[dep_idx] as usize;
            let dep_tags: Vec<String> = tags[tag_offset..tag_offset + tc]
                .iter()
                .map(|t| t.as_str().to_string())
                .collect();
            tag_offset += tc;
            deps.push((dependencies[dep_idx].as_str().to_string(), dep_tags));
        }
        dep_offset += dep_count;
        result.push((path.as_str().to_string(), deps));
    }

    Ok(result)
}
