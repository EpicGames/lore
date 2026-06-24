// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_proto::lore::storage::v1 as storage_v1;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::log_server_error;
use crate::grpc::simple_map_message_handle_error;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::verify::handle_verify;
use crate::util::setup_execution;

#[tracing::instrument(name = "StorageServiceV1::Verify", skip_all)]
pub async fn handler(
    request: Request<storage_v1::VerifyRequest>,
    local_immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<Response<storage_v1::VerifyResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(
            execution,
            async move {
                let req = request.into_inner();

                let address: Address = req
                    .address
                    .ok_or_else(|| Status::invalid_argument("Missing address"))?
                    .into();

                let heal_flag = if req.heal { 1 } else { 0 };

                handle_verify(
                    address,
                    heal_flag,
                    repository,
                    correlation_id,
                    user_id,
                    local_immutable_store,
                )
                .await
                .map(|resp| {
                    let LoreResponse::Verify(resp) = resp else {
                        panic!("Verify handler returned the wrong response type");
                    };

                    Response::new(storage_v1::VerifyResponse {
                        corrupted: resp.corrupted != 0,
                        healed: resp.healed as i32,
                    })
                })
                .map_err(simple_map_message_handle_error)
                .inspect_err(log_server_error)
            }
            .in_current_span(),
        )
        .await
}
