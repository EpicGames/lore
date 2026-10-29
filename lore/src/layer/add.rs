// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_revision::layer;
use lore_revision::layer::LayerError;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;

pub async fn add(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    mount_path: RelativePath,
    source_repository: &str,
    source_path: RelativePath,
    metadata: Option<String>,
) -> Result<(), LayerError> {
    let mut source_repository_id = RepositoryId::from_str(source_repository).unwrap_or_default();
    if source_repository_id.is_zero() {
        // Try resolving using repository service
        let remote = repository
            .remote()
            .await
            .forward::<LayerError>("Failed to resolve layer repository name")?;
        let repository_service = remote
            .repository()
            .await
            .forward::<LayerError>("Failed to resolve layer repository name")?;
        let response = repository_service
            .query(None, Some(source_repository))
            .await
            .forward::<LayerError>("Failed to resolve layer repository name")?;
        source_repository_id = response.id;
    }

    layer::add(
        repository,
        token,
        mount_path,
        source_repository_id,
        source_path,
        metadata.as_deref(),
    )
    .await?;

    // TODO(mjansson): Events

    Ok(())
}
