// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt;

use lore_base::types::RepositoryId;
use lore_proto::lore::user::v1::RepositoryRegisterRequest;
use lore_proto::lore::user::v1::RepositoryUnregisterRequest;
use lore_proto::rebac::CreateResourceRequest;
use lore_proto::rebac::DeleteResourceRequest;
use lore_revision::environment::EnvironmentConfig;
use tonic::Code;
use tonic::Status;
use tracing::info;

use super::auth::grpc_get_repository_registry_client;
use super::common::create_request_with_authorization;
use super::rebac::RebacApiClient;
use super::rebac::grpc_get_rebac_client;
use crate::grpc::ServerResultExt;
use crate::grpc::warn_error_to_status;

/// Where the server registers the repositories it creates and deletes, so
/// that the directory answering the repository catalog learns of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryRegistry {
    Unregistered,
    /// `RebacApi.CreateResource` / `DeleteResource` at this `auth_url`.
    AuthService(String),
    /// `lore.user.v1.RepositoryRegistryService.RepositoryRegister` /
    /// `RepositoryUnregister` at this `user_url`.
    UserService(String),
}

impl RepositoryRegistry {
    /// `user_url` if set, else `auth_url` if set, else no registration: the
    /// rule the client picks its user directory by and the catalog its mode.
    pub fn select(auth_url: Option<&str>, user_url: Option<&str>) -> Self {
        // An empty `user_url` counts as unset, as it does for the client and
        // the catalog.
        match (user_url.filter(|url| !url.is_empty()), auth_url) {
            (Some(user_url), _) => Self::UserService(user_url.to_string()),
            (None, Some(auth_url)) => Self::AuthService(auth_url.to_string()),
            (None, None) => Self::Unregistered,
        }
    }

    pub fn of(environment: &EnvironmentConfig) -> Self {
        let endpoint = environment.endpoint.as_ref();
        Self::select(
            endpoint.and_then(|endpoint| endpoint.auth_url.as_deref()),
            endpoint.and_then(|endpoint| endpoint.user_url.as_deref()),
        )
    }

    /// Registers a repository the server is about to create. Called before
    /// the repository is stored, so a refusal leaves no repository behind.
    pub async fn add(
        &self,
        authorization: Option<String>,
        repository_id: RepositoryId,
        name: &str,
    ) -> Result<(), Status> {
        match self {
            Self::Unregistered => Ok(()),
            Self::AuthService(auth_url) => {
                let client = Box::new(grpc_get_rebac_client(auth_url.clone()).await?);
                repository_create_auth_resource(client, authorization, repository_id, name).await
            }
            Self::UserService(user_url) => {
                info!("Repository register {repository_id} with name {name}");
                let mut client = grpc_get_repository_registry_client(user_url).await?;
                let request = create_request_with_authorization(
                    RepositoryRegisterRequest {
                        id: repository_id.into(),
                        name: name.to_string(),
                    },
                    authorization,
                )?;
                match client.repository_register(request).await {
                    // A repeated register under the same name answers `OK`, so
                    // `ALREADY_EXISTS` is the repository registered under another name.
                    Err(err) if err.code() == Code::AlreadyExists => {
                        info!(auth_error = ?err, requested_repo_id = %repository_id, "Repository registered under another name");
                        Err(Status::already_exists(format!(
                            "Repository {repository_id} is registered under another name than {name}"
                        )))
                    }
                    result => add_outcome(
                        result.map(drop),
                        repository_id,
                        name,
                        "repository registry RepositoryRegister",
                    ),
                }
            }
        }
    }

    /// Unregisters a repository the server is deleting. Called before the
    /// repository's records are removed.
    pub async fn remove(
        &self,
        authorization: Option<String>,
        repository_id: RepositoryId,
    ) -> Result<(), Status> {
        match self {
            Self::Unregistered => Ok(()),
            Self::AuthService(auth_url) => {
                info!("Repository delete auth resource for {repository_id}");
                let mut client = grpc_get_rebac_client(auth_url.clone()).await?;
                let request = create_request_with_authorization(
                    DeleteResourceRequest {
                        resource_id: format!("urc-{repository_id}"),
                    },
                    authorization,
                )?;
                remove_outcome(
                    client.delete_resource(request).await.map(drop),
                    "auth delete_resource",
                )
            }
            Self::UserService(user_url) => {
                info!("Repository unregister {repository_id}");
                let mut client = grpc_get_repository_registry_client(user_url).await?;
                let request = create_request_with_authorization(
                    RepositoryUnregisterRequest {
                        id: repository_id.into(),
                    },
                    authorization,
                )?;
                remove_outcome(
                    client.repository_unregister(request).await.map(drop),
                    "repository registry RepositoryUnregister",
                )
            }
        }
    }
}

impl fmt::Display for RepositoryRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unregistered => f.write_str("none"),
            Self::AuthService(url) => write!(f, "RebacApi({url})"),
            Self::UserService(url) => write!(f, "RepositoryRegistryService({url})"),
        }
    }
}

#[lore_macro::test_pub]
async fn repository_create_auth_resource(
    mut client: Box<dyn RebacApiClient + Send + Sync>,
    authorization: Option<String>,
    repository_id: RepositoryId,
    name: &str,
) -> Result<(), Status> {
    info!(
        "Repository create auth resource for {} with name {}",
        repository_id, name
    );

    let request = create_request_with_authorization(
        CreateResourceRequest {
            resource_id: format!("urc-{repository_id}"),
            resource_name: String::from(name),
        },
        authorization,
    )?;

    add_outcome(
        client.create_resource(request).await.map(drop),
        repository_id,
        name,
        "auth create_resource",
    )
}

/// What a registration's answer means for the create. A repository already
/// registered continues the create, so a retried create succeeds.
#[lore_macro::test_pub]
fn add_outcome(
    result: Result<(), Status>,
    repository_id: RepositoryId,
    name: &str,
    rpc: &str,
) -> Result<(), Status> {
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.code() == Code::AlreadyExists => {
            info!(auth_error = ?err, requested_repo_id = %repository_id, "Repository already registered, continuing");
            Ok(())
        }
        Err(err) if err.code() == Code::PermissionDenied => {
            info!(?err, "Repository registration failed - permission denied");
            Err(Status::permission_denied(
                "Failed to create repository, permission denied",
            ))
        }
        Err(err) if err.code() == Code::Unauthenticated => {
            info!(?err, "Repository registration failed - unauthenticated");
            Err(Status::unauthenticated(
                "Failed to create repository, reauthenticate",
            ))
        }
        Err(err) if err.code() == Code::NotFound => {
            // there is an issue with misbehaving clients who create external Auth resources but don't check
            // for a success response before calling RepositoryCreate (which in turn depends on those external
            // resources). Doing so results in Auth Service returning a NotFound error that should effectively be bubbled up
            // to the client
            info!(auth_error = ?err, "Repository registration failed because of Auth 'NotFound'");
            Err(Status::failed_precondition(
                "A required Auth entity was not found",
            ))
            // todo(plockhart): Once auth service supports Richer Error Model, change to look for an error code
        }
        Err(err)
            if err.code() == Code::InvalidArgument
                && err
                    .message()
                    .contains("Missing resource context in resourceName") =>
        {
            info!(auth_error = ?err, requested_name = name, "Repository registration failed - invalid name was provided");
            Err(Status::invalid_argument(
                "Invalid repository name - missing Organization context",
            ))
        }
        Err(err) => Err(warn_error_to_status(&err, |err| {
            Status::internal(format!("Failed to call {rpc}: {err}"))
        })),
    }
}

#[lore_macro::test_pub]
fn remove_outcome(result: Result<(), Status>, rpc: &str) -> Result<(), Status> {
    result.warn_map_err(|err| match err.code() {
        Code::PermissionDenied => Status::permission_denied("Delete resource denied"),
        Code::Unauthenticated => {
            Status::unauthenticated("Delete resource failed - unauthenticated")
        }
        _ => Status::internal(format!("Failed to call {rpc}: {err}")),
    })
}
