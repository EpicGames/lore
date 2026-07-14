// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::ID;
use super::RepositoryAccess;
use super::RepositoryContext;
use super::RepositoryError;
use super::RepositoryFormat;
use super::create_client_memory_stores;
use super::read_id_from_file;
use crate::event;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::protocol;
use crate::repository;
use crate::runtime::execution_context;

/// Descriptive data for a repository.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryDataEventData {
    /// Remote URL of the repository.
    pub remote_url: LoreString,
    /// Repository identifier.
    pub id: RepositoryId,
    /// Repository name.
    pub name: LoreString,
    /// Repository description.
    pub description: LoreString,
    /// Identifier of the default branch.
    pub default_branch: BranchId,
    /// Name of the default branch.
    pub default_branch_name: LoreString,
    /// Name of the user who created the repository.
    pub creator: LoreString,
    /// Creation time of the repository, in seconds since the Unix epoch.
    pub created: u64,
}

pub async fn info(repository_url: Option<&str>, identity: &str) -> Result<(), RepositoryError> {
    // Use the url of the working repo if the user didn't provide one.
    let repository_url = if let Some(repository_url) = repository_url {
        repository_url.to_owned()
    } else {
        let execution_context = execution_context();
        let repo_path = execution_context.globals().repository_path();

        let repo_context = read_id_from_file(
            PathBuf::from(repo_path)
                .join(RepositoryFormat::detect(std::path::Path::new(repo_path)).dot_dir())
                .join(ID),
        )
        .internal("Invalid repository path")?;

        let config = crate::repository::load_repository_config(repo_path)?;
        format!(
            "{}/{}",
            config
                .remote_url
                .ok_or_else(|| RepositoryError::internal("Invalid URL"))?,
            repo_context
        )
    };

    // Parse the URL
    let (remote_url, name) = repository::parse_url(&repository_url, false)?;

    let connection = protocol::connect(remote_url.as_str(), identity, RepositoryId::default())
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

    let data = repository_service
        .query(None, Some(name.as_str()))
        .await
        .forward::<RepositoryError>("Failed to list repositories")?;

    let (immutable_store, mutable_store) = create_client_memory_stores().await?;

    lore_debug!("Repository query returned {:?}", data);

    let remote = protocol::connect(remote_url.as_str(), identity, data.id)
        .await
        .forward_with::<RepositoryError, _>(|| {
            format!("Failed to connect to remote repository {remote_url}")
        })?;

    let repository = Arc::new(RepositoryContext::new(
        None,
        immutable_store,
        mutable_store,
        data.id,
        crate::instance::InstanceId::default(),
        Ok(remote),
        Arc::default(),
        super::RepositoryFormat::Lore,
    ));

    let metadata = repository::metadata(repository, data.metadata)
        .await
        .forward::<RepositoryError>("Failed to load repository metadata")?;

    event::LoreEvent::RepositoryData(LoreRepositoryDataEventData {
        remote_url: remote_url.into(),
        id: data.id,
        name: metadata.name.into(),
        description: metadata.description.into(),
        default_branch: metadata.default_branch,
        default_branch_name: metadata.default_branch_name.into(),
        creator: metadata.creator.into(),
        created: metadata.created,
    })
    .send();

    Ok(())
}

/// Load repository metadata from the working repository's local store, without
/// querying the remote for it.
///
/// Opens the working repository, resolves the metadata hash from the local
/// mutable store and deserializes the repository metadata fragment from the local
/// immutable store, then emits the same [`LoreEvent::RepositoryData`] event as the
/// remote [`info`] path. Because the repository metadata fragment is among the
/// first fragments written, this is a good probe that local fragments survive
/// aggressive eviction/compaction.
pub async fn info_local() -> Result<(), RepositoryError> {
    let repo_path = execution_context().globals().repository_path().to_string();
    let repository = repository::load_and_connect(&repo_path, RepositoryAccess::ReadOnly)
        .await
        .forward::<RepositoryError>("Failed to open repository")?;

    let metadata_hash = repository::metadata_hash(repository.clone())
        .await
        .forward::<RepositoryError>("Failed to load repository metadata")?;

    let metadata = repository::metadata(repository.clone(), metadata_hash)
        .await
        .forward::<RepositoryError>("Failed to load repository metadata")?;

    let remote_url = repository
        .require_path()
        .ok()
        .and_then(|path| repository::repository_remote(path.to_string_lossy()).ok())
        .unwrap_or_default();

    event::LoreEvent::RepositoryData(LoreRepositoryDataEventData {
        remote_url: remote_url.into(),
        id: repository.id,
        name: metadata.name.into(),
        description: metadata.description.into(),
        default_branch: metadata.default_branch,
        default_branch_name: metadata.default_branch_name.into(),
        creator: metadata.creator.into(),
        created: metadata.created,
    })
    .send();

    Ok(())
}
