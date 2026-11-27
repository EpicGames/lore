// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;
use lore_transport::RepositoryRenameError;

use super::RepositoryError;
use super::is_valid_name;
use super::reference::RepositoryReference;
use super::reference::instance_reference;
use super::reference::remote_reference;
use crate::errors::*;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::protocol;

/// The failures of a repository rename: a new name that isn't valid, those of finding and
/// reaching the repository, a remote that cannot rename repositories, and a rename the remote
/// can have stopped after its checks passed, which the same rename, run again, completes.
#[error_set]
pub enum RenameError {
    InvalidArguments,
    InvalidPath,
    NoRemote,
    NotFound,
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotSupported,
    Oversized,
    AddressNotFound,
    RepositoryRenameIncomplete,
}

/// Renames a repository on a remote to `name`. Its ID does not change.
///
/// `repository_url` is a full URL, or a bare name or ID on the remote of the instance at the
/// call's repository path. Without it, the rename addresses the repository of that instance. A
/// name is resolved to an ID before the rename, which addresses the repository by ID. A
/// `RepositoryRenameIncomplete` names the repository by its full URL and ID.
pub async fn rename(
    repository_url: Option<&str>,
    name: &str,
    identity: &str,
) -> Result<(), RenameError> {
    if !is_valid_name(name) {
        return Err(RenameError::from(InvalidArguments {
            reason: format!("'{name}' is not a valid repository name"),
        }));
    }

    let (remote_url, reference) = rename_target(
        repository_url,
        execution_context().globals().repository_path(),
    )
    .forward_any::<RenameError>("finding the repository to rename")?;

    let connection = protocol::connect(
        remote_url.as_str(),
        identity,
        RepositoryId::default(), /* No repository */
    )
    .await
    .forward_with::<RenameError, _>(|| {
        format!("Failed to connect to remote repository {remote_url}")
    })?;

    let repository_service = connection
        .repository()
        .await
        .forward_with::<RenameError, _>(|| {
            format!("Failed to connect to remote repository {remote_url}")
        })?;

    let id = reference
        .resolve(repository_service.as_ref())
        .await
        .forward::<RenameError>("resolving the repository to rename")?;

    if !execution_context().globals().dry_run() {
        repository_service
            .rename(id, name)
            .await
            .map_err(|err| rename_error(err, remote_url.as_str(), id))?;
    }

    Ok(())
}

/// The remote URL and the repository a rename addresses. `repository_url` is a full URL, or a
/// bare name or ID on the remote of the instance at `repository_path`, as for a delete. Without
/// it, the rename addresses the repository of that instance by its ID.
#[lore_macro::test_pub]
pub(crate) fn rename_target(
    repository_url: Option<&str>,
    repository_path: &str,
) -> Result<(String, RepositoryReference), RepositoryError> {
    match repository_url {
        Some(repository_url) => remote_reference(repository_url, repository_path),
        None => instance_reference(repository_path),
    }
}

/// The error of a rename of the repository `id` on `remote_url` that failed with `err`. A
/// `RepositoryRenameIncomplete` names the repository as `<remote_url>/<id>`, which resolves to it
/// on this remote whatever the caller's instance and whatever name the remote holds.
#[lore_macro::test_pub]
fn rename_error(err: RepositoryRenameError, remote_url: &str, id: RepositoryId) -> RenameError {
    match err {
        RepositoryRenameError::RepositoryRenameIncomplete(incomplete) => {
            RenameError::RepositoryRenameIncomplete(
                RepositoryRenameIncomplete {
                    repository: format!("{remote_url}/{id}"),
                }
                .chain_err(incomplete, "renaming the repository"),
            )
        }
        err => {
            let Err(err) = Err::<std::convert::Infallible, _>(err)
                .forward::<RenameError>("Failed to rename repository");
            err
        }
    }
}
