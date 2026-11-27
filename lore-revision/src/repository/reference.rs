// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::str::FromStr;

use lore_error_set::prelude::*;
use lore_transport::ProtocolError;
use lore_transport::Repository;

use super::ID;
use super::RepositoryError;
use super::get_dot_lore_path;
use super::load_repository_config;
use super::load_repository_config_from_dot_dir;
use super::parse_url;
use super::read_id_from_file;
use crate::errors::InvalidPath;
use crate::lore::RepositoryId;

/// A repository on a remote as a caller names it: by ID, or by a name the remote resolves.
#[lore_macro::test_pub]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepositoryReference {
    Id(RepositoryId),
    Name(String),
}

impl RepositoryReference {
    /// `name` as an ID where it parses as one, and as a name otherwise.
    fn parse(name: &str) -> Self {
        match RepositoryId::from_str(name) {
            Ok(id) if !id.is_zero() => Self::Id(id),
            _ => Self::Name(name.to_string()),
        }
    }

    /// The ID of the repository, which `service` looks up by name for a name. An ID is used as
    /// is, without a lookup.
    pub(crate) async fn resolve(
        self,
        service: &dyn Repository,
    ) -> Result<RepositoryId, ProtocolError> {
        match self {
            Self::Id(id) => Ok(id),
            Self::Name(name) => Ok(service
                .query(None, Some(name.as_str()))
                .await
                .forward::<ProtocolError>("looking up the repository by name")?
                .id),
        }
    }
}

/// The remote URL and the repository that `repository_url` names. It is a full URL, or a bare
/// name or ID of a repository on the remote of the instance at `repository_path`.
///
/// Only a scheme tells the two apart: `is_valid_name` permits scoped names like `org/project`,
/// so a slash does not mean the first segment is a host. An instance without a remote to resolve
/// a bare name or ID against is `NoRemote`, not a malformed URL.
#[lore_macro::test_pub]
pub(crate) fn remote_reference(
    repository_url: &str,
    repository_path: &str,
) -> Result<(String, RepositoryReference), RepositoryError> {
    let (remote_url, name) = if repository_url.contains("://") {
        parse_url(repository_url, false)?
    } else {
        let remote_url = load_repository_config(repository_path)
            .ok()
            .and_then(|config| config.remote_url)
            .unwrap_or_default();
        if remote_url.is_empty() {
            return Err(RepositoryError::from(crate::errors::NoRemote));
        }
        (remote_url, repository_url.to_string())
    };
    Ok((remote_url, RepositoryReference::parse(name.as_str())))
}

/// The remote URL and the ID of the repository of the instance at `repository_path`, as its
/// Lore directory records them. A path that holds no instance is `InvalidPath`, and an instance
/// without a remote is `NoRemote`.
#[lore_macro::test_pub]
pub(crate) fn instance_reference(
    repository_path: &str,
) -> Result<(String, RepositoryReference), RepositoryError> {
    let dot_lore_path = get_dot_lore_path(Path::new(repository_path))
        .forward::<RepositoryError>("finding the Lore directory")?;
    let id = read_id_from_file(dot_lore_path.join(ID)).map_err(|err| {
        let mut error = RepositoryError::from(InvalidPath {
            path: repository_path.to_string(),
        });
        error.push_context(format!("reading the repository ID of the instance: {err}"));
        error
    })?;
    let remote_url = load_repository_config_from_dot_dir(&dot_lore_path)?
        .remote_url
        .unwrap_or_default();
    if remote_url.is_empty() {
        return Err(RepositoryError::from(crate::errors::NoRemote));
    }
    Ok((remote_url, RepositoryReference::Id(id)))
}
