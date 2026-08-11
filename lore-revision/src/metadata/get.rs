// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::MetadataErrors;
use crate::error::LoreResultExt;
use crate::errors::InvalidArguments;
use crate::event;
use crate::lore::execution_context;
use crate::metadata;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::util::path::RelativePath;

pub async fn get_revision(
    repository: Arc<RepositoryContext>,
    revision: Option<String>,
    key: &str,
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

    if let Some(metadata) = metadata::find::revision(repository.clone(), revision).await? {
        event::metadata::send_keyed(&metadata, key);
    }

    Ok(())
}

pub async fn get_file(
    repository: Arc<RepositoryContext>,
    revision: Option<String>,
    path: String,
    key: String,
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
        event::metadata::send_keyed(&metadata, key.as_str());
    }

    Ok(())
}
