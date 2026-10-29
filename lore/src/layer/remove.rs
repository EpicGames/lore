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

pub async fn remove(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    mount_path: RelativePath,
    source_repository: &str,
    purge: bool,
) -> Result<(), LayerError> {
    let source_repository_id = if source_repository.is_empty() {
        // Caller did not specify a source repository; core will resolve the
        // layer by mount_path alone (and error if multiple layers share it).
        RepositoryId::default()
    } else {
        let mut id = RepositoryId::from_str(source_repository).unwrap_or_default();
        if id.is_zero() {
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
            id = response.id;
        }
        id
    };

    layer::remove(repository, token, mount_path, source_repository_id, purge).await
}
