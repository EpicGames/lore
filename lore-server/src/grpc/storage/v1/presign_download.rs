// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::log_server_error;
use crate::util::setup_execution;

const DEFAULT_PRESIGN_EXPIRES_IN_SECONDS: u64 = 300;
const MAX_PRESIGN_EXPIRES_IN_SECONDS: u64 = 3600;

fn map_store_error(err: StoreError) -> Status {
    match err {
        StoreError::AddressNotFound(_) => Status::not_found("fragment not found"),
        StoreError::SlowDown(_) => Status::resource_exhausted("server overloaded, slow down"),
        StoreError::Oversized(_) => Status::out_of_range(err.to_string()),
        StoreError::NotAuthorized(_) => Status::permission_denied(err.to_string()),
        StoreError::NotAuthenticated(_) => Status::unauthenticated(err.to_string()),
        StoreError::Maintenance(_) | StoreError::Disconnected(_) => {
            Status::unavailable(err.to_string())
        }
        StoreError::NotSupported(_) => Status::unimplemented(err.to_string()),
        StoreError::NoRemote(_)
        | StoreError::NotFound(_)
        | StoreError::PayloadNotFound(_)
        | StoreError::Internal(_) => Status::internal(err.to_string()),
    }
}

#[tracing::instrument(name = "StorageServiceV1::PresignDownload", skip_all)]
pub async fn handler(
    request: Request<storage_v1::PresignDownloadRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<Response<storage_v1::PresignDownloadResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    LORE_CONTEXT
        .scope(
            execution,
            async move {
                let req = request.into_inner();

                if req.addresses.len() > crate::protocol::storage::query::MAX_FRAGMENTS {
                    return Err(Status::invalid_argument(format!(
                        "too many addresses: {} exceeds limit {}",
                        req.addresses.len(),
                        crate::protocol::storage::query::MAX_FRAGMENTS,
                    )));
                }

                let expires_in_seconds = if req.expires_in_seconds == 0 {
                    DEFAULT_PRESIGN_EXPIRES_IN_SECONDS
                } else {
                    req.expires_in_seconds.min(MAX_PRESIGN_EXPIRES_IN_SECONDS)
                };

                let addresses: Vec<Address> = req.addresses.iter().map(Address::from).collect();
                let downloads = immutable_store
                    .presign_downloads(
                        repository,
                        &addresses,
                        StoreMatch::MatchFull,
                        Duration::from_secs(expires_in_seconds),
                    )
                    .await
                    .map_err(map_store_error)
                    .inspect_err(log_server_error)?;

                let downloads = downloads
                    .into_iter()
                    .map(|download| storage_v1::PresignedDownload {
                        address: Some(download.address.into()),
                        fragment: Some(download.fragment.into()),
                        url: download.url,
                        expires_at_epoch_seconds: download.expires_at_epoch_seconds,
                    })
                    .collect();

                Ok(Response::new(storage_v1::PresignDownloadResponse {
                    downloads,
                }))
            }
            .in_current_span(),
        )
        .await
}
