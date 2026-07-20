// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::immutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::execution_context;
use crate::node::NodeFileMode;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::state;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::is_path_inside_repository;

/// Data for the event emitted when file content is written to a destination.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileWriteEventData {
    /// Path that was written.
    pub path: LoreString,
}

#[error_set]
pub enum WriteError {
    InvalidArguments,
    InvalidPath,
    InvalidAddress,
    RevisionNotFound,
    FileNotFound,
    WriteRequired,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for WriteError {
    fn translated(&self) -> LoreError {
        match self {
            WriteError::InvalidArguments(_)
            | WriteError::InvalidPath(_)
            | WriteError::InvalidAddress(_) => LoreError::InvalidArguments,
            WriteError::RevisionNotFound(_) | WriteError::NotFound(_) => LoreError::NotFound,
            WriteError::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct WriteFileOptions {
    /// Optional revision signature
    pub revision: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WriteAddressOptions {}

/// Per the `lore-revision/clippy.toml` disallow-list policy, repository-level
/// filesystem writes must hold a `RepositoryWriteToken`. The output
/// destination of `write_{file,address}` is the only thing they mutate, so
/// the discipline reduces to: token present, OR destination outside the
/// repository working directory.
fn check_destination_access(
    repository_path: &Path,
    output: &str,
    token: Option<&RepositoryWriteToken>,
) -> Result<(), WriteError> {
    if token.is_some() {
        return Ok(());
    }
    if is_path_inside_repository(repository_path, output) {
        return Err(WriteRequired.into());
    }
    Ok(())
}

pub async fn write_file(
    repository: Arc<RepositoryContext>,
    token: Option<&RepositoryWriteToken>,
    path: String,
    output: String,
    options: WriteFileOptions,
) -> Result<(), WriteError> {
    check_destination_access(repository.require_path()?, output.as_str(), token)?;

    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<WriteError>("resolving user path")?;

    let signature = if let Some(revision) = options.revision {
        revision::resolve(
            repository.clone(),
            revision.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            WriteError::from(RevisionNotFound {
                revision: revision.clone(),
            })
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<WriteError>("Failed to deserialize current revision anchor")?;
        crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .unwrap_or(current_revision)
    };

    let destination = {
        let mut absolute_path = Path::new(&output).to_path_buf();
        if !absolute_path.is_absolute() {
            let Ok(resolved) = crate::util::path::make_absolute(&output) else {
                return Err(InvalidPath {
                    path: output.clone(),
                }
                .into());
            };

            absolute_path = resolved;
        }
        absolute_path
    };

    match tokio::fs::metadata(&destination).await {
        Ok(metadata) => {
            if metadata.is_dir() {
                return Err(InvalidPath {
                    path: destination.display().to_string(),
                }
                .into());
            }
            if metadata.is_file() && !execution_context().globals().force() {
                return Err(InvalidPath {
                    path: destination.display().to_string(),
                }
                .into());
            }
        }
        Err(err) => {
            if err.kind() != tokio::io::ErrorKind::NotFound {
                return Err(WriteError::internal_with_context(
                    err,
                    "checking output destination",
                ));
            }
        }
    }

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<WriteError>("Failed to deserialize state")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_err(|_err| {
            WriteError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;
    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: relative_path.to_string(),
        }
        .into());
    }

    let node = state
        .node(repository.clone(), node_link.node)
        .await
        .map_err(|_err| {
            WriteError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    if !node.is_file() {
        return Err(FileNotFound {
            resource: relative_path.to_string(),
        }
        .into());
    }

    if node.size > 0 {
        let _ = immutable::read_into_file(
            repository.clone(),
            node.address,
            destination.as_path(),
            immutable::read_options_from_repository(&repository),
        )
        .await
        .forward::<WriteError>("Failed to write file")?;
    } else {
        // Zero sized file, just create
        tokio::fs::OpenOptions::new()
            .read(false)
            .write(true)
            .truncate(true)
            .create(true)
            .open(destination.as_path())
            .await
            .internal("Failed to write file")?;
    }

    let metadata = tokio::fs::metadata(destination.as_path())
        .await
        .internal("Failed to write file")?;

    let node_executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
    if node_executable != util::fs::file_is_executable(&metadata) {
        util::fs::metadata_set_executable(destination.as_path(), &metadata, node_executable).await;
    }

    event::LoreEvent::FileWrite(LoreFileWriteEventData {
        path: destination.into(),
    })
    .send();

    Ok(())
}

pub async fn write_address(
    repository: Arc<RepositoryContext>,
    token: Option<&RepositoryWriteToken>,
    address: String,
    output: String,
    _options: WriteAddressOptions,
) -> Result<(), WriteError> {
    check_destination_access(repository.require_path()?, output.as_str(), token)?;

    let address_value = Address::from_str(&address).map_err(|_err| {
        WriteError::from(InvalidAddress {
            address: address.clone(),
        })
    })?;

    let destination = {
        let mut absolute_path = Path::new(&output).to_path_buf();
        if !absolute_path.is_absolute() {
            let Ok(resolved) = crate::util::path::make_absolute(&output) else {
                return Err(InvalidPath {
                    path: output.clone(),
                }
                .into());
            };

            absolute_path = resolved;
        }
        absolute_path
    };

    match tokio::fs::metadata(&destination).await {
        Ok(metadata) => {
            if metadata.is_dir() {
                return Err(InvalidPath {
                    path: destination.display().to_string(),
                }
                .into());
            }
            if metadata.is_file() && !execution_context().globals().force() {
                return Err(InvalidPath {
                    path: destination.display().to_string(),
                }
                .into());
            }
        }
        Err(err) => {
            if err.kind() != tokio::io::ErrorKind::NotFound {
                return Err(WriteError::internal_with_context(
                    err,
                    "checking output destination",
                ));
            }
        }
    }

    let _ = immutable::read_into_file(
        repository.clone(),
        address_value,
        destination.as_path(),
        immutable::read_options_from_repository(&repository),
    )
    .await
    .forward::<WriteError>("Failed to write file")?;

    event::LoreEvent::FileWrite(LoreFileWriteEventData {
        path: destination.into(),
    })
    .send();

    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "windows"))]
    use super::*;

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn destination_inside_repo_without_token_is_write_required() {
        let result = check_destination_access(Path::new("/a/b"), "/a/b/payload.bin", None);
        assert!(matches!(result, Err(WriteError::WriteRequired(_))));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn destination_outside_repo_without_token_is_ok() {
        let result = check_destination_access(Path::new("/a/b"), "/c/payload.bin", None);
        assert!(result.is_ok());
    }
}
