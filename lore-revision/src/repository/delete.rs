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
    // The argument is a full URL, or a bare name or ID naming a repository on the remote
    // this working copy already points at — `delete <id>` after reading the id out of the
    // repository being deleted is the common form. Only a scheme separates the two:
    // `is_valid_name` permits scoped names like `org/project`, so a slash does not mean the
    // first segment is a host. Resolve every schemeless identifier against this working
    // copy's config rather than making the caller repeat the host, and report having no
    // remote to resolve it against as `NoRemote` rather than as a malformed URL.
    let (remote_url, name) = if repository_url.contains("://") {
        repository::parse_url(repository_url, false)?
    } else {
        let context = execution_context();
        let repository_path = context.globals().repository_path();
        let remote_url = repository::load_repository_config(repository_path)
            .ok()
            .and_then(|config| config.remote_url)
            .unwrap_or_default();
        if remote_url.is_empty() {
            return Err(RepositoryError::from(crate::errors::NoRemote));
        }
        (remote_url, repository_url.to_string())
    };

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
