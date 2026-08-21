// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use lore_error_set::prelude::*;

use super::RepositoryError;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::protocol;
use crate::repository;

pub async fn delete(repository_url: &str, identity: &str) -> Result<(), RepositoryError> {
    let (remote_url, name) = repository::parse_url(repository_url, false)?;

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

    let mut id = RepositoryId::from_str(name.as_str()).unwrap_or_default();

    if id.is_zero() {
        let data = repository_service
            .query(None, Some(name.as_str()))
            .await
            .forward::<RepositoryError>(
                "Invalid repository name, can only contain alphanumerical characters and separators /-_",
            )?;
        id = data.id;
    }

    if !execution_context().globals().dry_run() {
        repository_service
            .delete(id)
            .await
            .forward::<RepositoryError>("Failed to delete repository")?;
    }

    Ok(())
}
