// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::MetadataErrors;
use crate::error::LoreResultExt;
use crate::errors::InvalidArguments;
use crate::event;
use crate::metadata;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::runtime::execution_context;
use crate::util::path::RelativePath;

pub async fn list_revision(
    repository: Arc<RepositoryContext>,
    revision: Option<String>,
) -> Result<(), MetadataErrors> {
    let signature = if let Some(revision) = revision {
        revision::resolve(
            repository.clone(),
            revision,
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .emit_map_err(InvalidArguments {
            reason: "invalid revision".into(),
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .internal("deserializing current anchor")?;
        crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .unwrap_or(current_revision)
    };

    if let Some(metadata) = metadata::find::revision(repository.clone(), signature).await? {
        event::metadata::send(&metadata);
    }

    Ok(())
}

pub async fn list_file(
    repository: Arc<RepositoryContext>,
    revision: Option<String>,
    path: String,
) -> Result<(), MetadataErrors> {
    let revision = if let Some(revision) = revision {
        revision::resolve(
            repository.clone(),
            revision,
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .emit_map_err(InvalidArguments {
            reason: "invalid revision".into(),
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .internal("deserializing current anchor")?;
        crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .unwrap_or(current_revision)
    };

    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<MetadataErrors>("resolving user path")?;

    if let Some(metadata) =
        metadata::find::file(repository.clone(), revision, &relative_path).await?
    {
        event::metadata::send(&metadata);
    }

    Ok(())
}
