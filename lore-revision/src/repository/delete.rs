// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;

use super::RepositoryError;
use super::reference::remote_reference;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::protocol;

pub async fn delete(repository_url: &str, identity: &str) -> Result<(), RepositoryError> {
    let (remote_url, reference) = remote_reference(
        repository_url,
        execution_context().globals().repository_path(),
    )?;

    let connection = protocol::connect(
        remote_url.as_str(),
        identity,
        RepositoryId::default(), /* No repository */
    )
    .await
    .forward_with::<RepositoryError, _>(|| {
        format!("Failed to connect to remote repository {remote_url}")
    })?;

    let repository_service = connection
        .repository()
        .await
        .forward_with::<RepositoryError, _>(|| {
            format!("Failed to connect to remote repository {remote_url}")
        })?;

    let id = reference
        .resolve(repository_service.as_ref())
        .await
        .forward::<RepositoryError>("resolving the repository to delete")?;

    if !execution_context().globals().dry_run() {
        repository_service
            .delete(id)
            .await
            .forward::<RepositoryError>("Failed to delete repository")?;
    }

    Ok(())
}
