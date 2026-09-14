// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::errors::InvalidArguments;
use crate::event::LoreEvent;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::util::path::RelativePath;

/// Event data naming a path that was ignored or could not be resolved.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LorePathIgnoreEventData {
    /// The ignored path
    pub path: LoreString,
}

pub async fn emit_path_ignore(path: &str) {
    LoreEvent::PathIgnore(LorePathIgnoreEventData { path: path.into() }).send();
}

/// Resolve user-supplied paths against the repository root, reporting and dropping the ones
/// that name nothing inside it.
///
/// Callers that route paths before walking them need the resolved form up front, so this
/// happens once rather than inside each walk.
pub async fn resolve_user_paths(
    repository: &Arc<RepositoryContext>,
    paths: &LoreArray<LoreString>,
) -> Result<Vec<RelativePath>, InvalidArguments> {
    let root = repository.require_path()?;
    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths.as_slice().iter() {
        let Ok(relative_path) = RelativePath::new_from_user_path(root, path.as_str()) else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path: {path}");
            continue;
        };

        lore_debug!(
            "User path [{}] transformed to relative path [{}] in repository {}",
            path.as_str(),
            relative_path.as_str(),
            repository.path_for_display()
        );
        resolved.push(relative_path);
    }
    Ok(resolved)
}
