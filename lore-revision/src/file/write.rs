// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::set_file_to_node;
use crate::fs::filesystem_provider::with_operation;
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
use crate::util::path::repository_relative_path;

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

/// Where `output` names, and the path the working tree holds it at where it holds it at all.
///
/// A destination the tree holds is written through an operation on it, so a virtual filesystem
/// answers for it. One outside the root, or under the dot directory, is written to the host
/// filesystem as named.
fn write_destination(
    repository_path: &Path,
    output: &str,
) -> Result<(PathBuf, Option<RelativePath>), WriteError> {
    let destination = if Path::new(output).is_absolute() {
        PathBuf::from(output)
    } else {
        crate::util::path::make_absolute(output).map_err(|_err| InvalidPath {
            path: output.to_string(),
        })?
    };
    Ok((
        destination,
        repository_relative_path(repository_path, output),
    ))
}

/// Refuses a destination a write must not replace.
///
/// A directory is never one. A file already there is one only under `--force`, so a write never
/// silently replaces content the user named only the destination of.
fn require_free_destination(held: FileInfo, destination: &Path) -> Result<(), WriteError> {
    let occupied = match held {
        FileInfo::NotExist => false,
        FileInfo::Directory => true,
        FileInfo::File { .. } => !execution_context().globals().force(),
    };
    if occupied {
        return Err(InvalidPath {
            path: destination.display().to_string(),
        }
        .into());
    }
    Ok(())
}

/// What the host filesystem holds at `destination`, for a destination no operation covers.
async fn host_file_info(destination: &Path) -> Result<FileInfo, WriteError> {
    match lore_io::IoDriver::global().metadata(destination).await {
        Ok(metadata) => Ok(FileInfo::from_metadata(&metadata)),
        Err(err) if err.kind() == tokio::io::ErrorKind::NotFound => Ok(FileInfo::NotExist),
        Err(err) => Err(WriteError::internal_with_context(
            err,
            "checking output destination",
        )),
    }
}

pub(crate) async fn write_file(
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

    let (destination, tracked) = write_destination(repository.require_path()?, output.as_str())?;

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

    if let Some(tracked) = &tracked {
        with_operation(repository.file_system(), async |operation| {
            require_free_destination(
                operation
                    .file_info(tracked)
                    .await
                    .forward_any::<WriteError>("Failed to check the output destination")?,
                &destination,
            )?;
            set_file_to_node::<WriteError>(&operation, repository.clone(), &node, tracked)
                .await
                .map(|_written| ())
        })
        .await?;
    } else {
        require_free_destination(host_file_info(&destination).await?, &destination)?;
        let written_metadata = immutable::read_into_file(
            repository.clone(),
            node.address,
            destination.as_path(),
            None,
            immutable::read_options_from_repository(&repository),
        )
        .await
        .forward::<WriteError>("Failed to write file")?
        .1;

        // Taken from the write where it captured one on the open handle. The multi-fragment
        // path surfaces none, since the handle travels through the defragment pipeline, so
        // that case is the one that still asks.
        let metadata = match written_metadata {
            Some(metadata) => metadata,
            None => lore_io::IoDriver::global()
                .metadata(destination.as_path())
                .await
                .internal("Failed to write file")?,
        };

        let node_executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
        if node_executable != util::fs::file_is_executable(&metadata) {
            util::fs::metadata_set_executable(destination.as_path(), &metadata, node_executable)
                .await;
        }
    }

    event::LoreEvent::FileWrite(LoreFileWriteEventData {
        path: destination.into(),
    })
    .send();

    Ok(())
}

/// Boxed version of [`write_file`] for cross-crate use.
pub fn write_file_boxed(
    repository: Arc<RepositoryContext>,
    token: Option<&RepositoryWriteToken>,
    path: String,
    output: String,
    options: WriteFileOptions,
) -> crate::BoxFuture<'_, Result<(), WriteError>> {
    Box::pin(write_file(repository, token, path, output, options))
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

    let (destination, tracked) = write_destination(repository.require_path()?, output.as_str())?;

    if let Some(tracked) = &tracked {
        with_operation::<_, WriteError, _>(repository.file_system(), async |operation| {
            require_free_destination(
                operation
                    .file_info(tracked)
                    .await
                    .forward_any::<WriteError>("Failed to check the output destination")?,
                &destination,
            )?;
            operation
                .set_file_to_immutable_store_contents(repository.clone(), address_value, tracked)
                .await
                .forward_any::<WriteError>("Failed to write file")?;
            Ok(())
        })
        .await?;
    } else {
        require_free_destination(host_file_info(&destination).await?, &destination)?;
        immutable::read_into_file(
            repository.clone(),
            address_value,
            destination.as_path(),
            None,
            immutable::read_options_from_repository(&repository),
        )
        .await
        .forward::<WriteError>("Failed to write file")?;
    }

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
