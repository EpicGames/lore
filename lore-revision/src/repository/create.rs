// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::FileConfig;
use super::RepositoryConfig;
use super::RepositoryMetadata;
use super::RepositoryWriteToken;
use super::SharedStoreToUseConfig;
use super::StoreConfig;
use super::VfsConfig;
use super::get_dot_lore_path;
use crate::branch;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::hash;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Context;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::protocol;
use crate::repository;
use crate::util;

/// Data for the event emitted when a repository is created.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryCreateEventData {
    /// Identifier of the created repository.
    pub id: RepositoryId,
    /// Name of the created repository.
    pub name: LoreString,
    /// Local path of the created repository.
    pub path: LoreString,
}

#[error_set]
pub enum CreateError {
    RepositoryAlreadyExists,
    InvalidPath,
    Disconnected,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    Maintenance,
    NotFound,
    NotSupported,
    Oversized,
    SlowDown,
    AddressNotFound,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    InvalidArguments,
    InvalidNodeHierarchy,
    LayerNotFound,
    LinkNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NodeNotFound,
    NotALayer,
    NotALink,
    NotConnected,
    NothingStaged,
    PayloadNotFound,
    RepositoryNotFound,
    RevisionNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    WriteRequired,
    MissingIdentity,
}

impl EventError for CreateError {
    fn translated(&self) -> LoreError {
        match self {
            CreateError::Disconnected(_) => LoreError::Connection,
            CreateError::SlowDown(_) => LoreError::SlowDown,
            CreateError::Oversized(_) => LoreError::Oversized,
            CreateError::NotFound(_) => LoreError::NotFound,
            CreateError::RepositoryAlreadyExists(_) => LoreError::AlreadyExists,
            CreateError::InvalidPath(_) => LoreError::InvalidArguments,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
    // Repository id
    pub id: Option<RepositoryId>,
    // Repository description
    pub description: Option<String>,
    // Whether to use the shared store and options configuring it if desired
    pub shared_store_options: Option<SharedStoreToUseConfig>,
    // Whether to use VFS for the repository
    pub vfs_options: VfsConfig,
}

#[derive(Clone)]
pub struct CreateMetadata {
    // Creator
    pub creator: String,
    // Created
    pub created: u64,
}

/// Names a repository created without a URL after the directory it lives in.
///
/// The path reaching here may still be relative (`.` is the common case), so resolve it
/// against the call's working directory before taking the final component — otherwise
/// every repository created in the current directory would be named `.`.
fn directory_name(path: &Path) -> Option<String> {
    let absolute = path
        .to_str()
        .and_then(|path| util::path::make_absolute(path).ok())
        .unwrap_or_else(|| path.to_path_buf());
    absolute
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

pub async fn create(
    repository_url: &str,
    path: impl AsRef<Path>,
    options: CreateOptions,
) -> Result<(), CreateError> {
    create_with_metadata(repository_url, path, options, None).await
}

pub async fn create_with_metadata(
    repository_url: &str,
    path: impl AsRef<Path>,
    options: CreateOptions,
    metadata: Option<CreateMetadata>,
) -> Result<(), CreateError> {
    let context = execution_context();
    let call = context.globals();

    let path = path.as_ref();

    // A local repository is the only kind that has no remote, and `--offline` / `--local`
    // is how the caller asks for one. Under that flag the argument names the repository
    // rather than locating it, and may be omitted entirely for the directory's own name.
    // Without it a URL that names a host is required, so a bare name is never silently
    // reinterpreted as a repository name.
    let local_only = call.offline_or_local();
    let (remote_url, name) = if repository_url.is_empty() {
        if !local_only {
            return Err(InvalidArguments {
                reason: "a repository URL is required, pass --offline to create a local \
                         repository named after the current directory"
                    .to_string(),
            }
            .into());
        }
        let name = directory_name(path)
            .ok_or_else(|| CreateError::internal("Unable to derive repository name from path"))?;
        (String::default(), name)
    } else {
        // A bare name is only a name when the caller asked for a local repository. The
        // check is here rather than left to `parse_url` so the message names the choice
        // the caller has, instead of reporting their repository name as a broken URL.
        if !local_only && !repository_url.contains("://") && !repository_url.contains('/') {
            return Err(InvalidArguments {
                reason: format!(
                    "repository URL '{repository_url}' must include a host name, pass \
                     --offline to create a local repository named '{repository_url}'"
                ),
            }
            .into());
        }
        repository::parse_url(repository_url, local_only)
            .forward::<CreateError>("parsing repository URL")?
    };

    if !repository::is_valid_name(name.as_str()) {
        // Blame whatever the name actually came from. Without an argument it came from the
        // directory; under `--offline` the argument is the name itself, so calling it a
        // broken URL would point the reader at a scheme they deliberately did not pass.
        return Err(CreateError::internal(if repository_url.is_empty() {
            format!("Directory name '{name}' is not a valid repository name, pass a name or URL")
        } else if local_only {
            format!(
                "'{name}' is not a valid repository name, names may hold letters, digits, \
                 '/', '-', '_' and '.'; include a scheme such as lore:// to name a remote"
            )
        } else {
            "Invalid repository URL".to_string()
        }));
    }

    let existing_dot_dir = get_dot_lore_path(path)?;
    if existing_dot_dir.exists() {
        if call.force() {
            lore_io::IoDriver::global()
                .remove_dir_all(existing_dot_dir.as_path())
                .await
                .internal_with(|| {
                    format!("removing previous repository in path {}", path.display())
                })?;
        } else {
            return Err(CreateError::from(RepositoryAlreadyExists {
                path: path.display().to_string(),
            }));
        }
    }

    let id = options.id.unwrap_or_else(|| uuid::Uuid::now_v7().into());

    // TODO(mjansson): Make this configurable in arguments and command line
    let branch_name = branch::DEFAULT_DEFAULT_NAME;
    // Use the hash of the default name for the main branch ID to make it
    // easily distinguishable in logs even in the ID form.
    let branch = if branch_name == branch::DEFAULT_DEFAULT_NAME {
        hash::hash_slice(branch_name.as_bytes()).to_context()
    } else {
        Context::from(uuid::Uuid::now_v7())
    };

    // With no remote URL there is no server to create the repository on, so skip
    // straight to the local setup rather than failing the way an unreachable remote does.
    let connection = if !call.offline_or_local() && !remote_url.is_empty() {
        // Try to create the repository on server
        let connection = protocol::connect(
            remote_url.as_str(),
            call.identity().unwrap_or_default(),
            RepositoryId::default(), /* No repository */
        )
        .await
        .forward::<CreateError>("connecting to remote")?;

        let (creator, created) = {
            if let Some(metadata) = metadata.clone() {
                (metadata.creator, metadata.created)
            } else {
                (connection.identity.clone(), util::time::timestamp())
            }
        };

        let repository_service = connection
            .repository()
            .await
            .forward::<CreateError>("acquiring repository service")?;
        repository_service
            .create(
                id,
                name.as_str(),
                options.description.as_deref().unwrap_or_default(),
                branch,
                branch_name,
                creator.as_str(),
                created,
            )
            .await
            .forward::<CreateError>("creating repository on server")?;

        Some(connection)
    } else {
        None
    };

    let (creator, created) = {
        if let Some(metadata) = metadata.clone() {
            (metadata.creator, metadata.created)
        } else {
            (
                call.identity().unwrap_or_default().to_string(),
                util::time::timestamp(),
            )
        }
    };

    let metadata = RepositoryMetadata {
        name: name.clone(),
        description: options.description.unwrap_or_default(),
        default_branch: branch,
        default_branch_name: branch_name.to_string(),
        creator,
        created,
    };
    let resolved_identity = if let Some(cli_identity) = call.identity() {
        Some(cli_identity.to_string())
    } else if let Some(ref conn) = connection {
        (!conn.identity.is_empty()).then(|| conn.identity.clone())
    } else {
        None
    };

    let config = RepositoryConfig {
        remote_url: Some(remote_url),
        identity: resolved_identity,
        shared_store_to_use: options.shared_store_options,
        vfs: Some(options.vfs_options),
        store: Some(StoreConfig::client_default()),
        file: Some(FileConfig::default()),
    };

    // `create` is a genesis-path write command that doesn't flow through
    // `repository_call`, so acquire the per-path write mutex here. The token
    // moves into the returned context and keeps the mutex held across the
    // subsequent metadata / name writes until `repository` drops below.
    let write_token = RepositoryWriteToken::acquire(path).await;
    let repository = repository::create_local(
        path,
        &write_token,
        id,
        metadata.default_branch,
        metadata.default_branch_name.clone(),
        config,
        false, /* Full local tracking */
    )
    .await
    .forward::<CreateError>("creating local repository on disk")?;

    // Always store metadata locally so that the default branch can be
    // resolved without remote connectivity (e.g. offline stage/commit).
    {
        let metadata_hash = repository::metadata_store(repository.clone(), metadata)
            .await
            .forward::<CreateError>("storing metadata")?;
        repository::metadata_store_hash(repository.clone(), metadata_hash)
            .await
            .forward::<CreateError>("storing metadata hash")?;
        repository::store_name_to_id(repository.clone(), name.as_str(), repository.id)
            .await
            .forward::<CreateError>("storing name-to-id mapping")?;
    }

    // The create command goes through dispatch_call, not repository_call,
    // so the normal spawn_flush_stores path that flushes dirty mutable store
    // buckets after each command does not run. Flush explicitly to ensure
    // branch creation, instance registration, and metadata are persisted
    // before the process exits.
    let _ = repository.flush(true).await;

    drop(repository);
    drop(connection);

    event::LoreEvent::RepositoryCreate(LoreRepositoryCreateEventData {
        id,
        name: name.into(),
        path: path.into(),
    })
    .send();

    Ok(())
}
