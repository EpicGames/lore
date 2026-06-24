// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_proto::lore::storage::v1 as storage_v1;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::log_server_error;
use crate::grpc::simple_map_message_handle_error;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::mutable_cas::handle_mutable_cas;
use crate::util::setup_execution;

#[tracing::instrument(name = "StorageServiceV1::MutableCompareAndSwap", skip_all)]
pub async fn handler(
    request: Request<storage_v1::MutableCompareAndSwapRequest>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<storage_v1::MutableCompareAndSwapResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            let req = request.into_inner();

            let key = Hash::from(&req.key[..]);
            let expected = Hash::from(&req.expected[..]);
            let value = Hash::from(&req.value[..]);
            let key_type = KeyType::try_from(req.key_type).map_err(|_err| {
                Status::invalid_argument(format!("Invalid key_type: {}", req.key_type))
            })?;

            handle_mutable_cas(
                key,
                expected,
                value,
                key_type,
                repository,
                correlation_id,
                user_id,
                mutable_store,
            )
            .await
            .map(|resp| {
                let LoreResponse::MutableCas(resp) = resp else {
                    panic!("MutableCompareAndSwap handler returned the wrong response type");
                };
                Response::new(storage_v1::MutableCompareAndSwapResponse {
                    current_value: bytes::Bytes::copy_from_slice(resp.current_value.as_ref()),
                })
            })
            .map_err(simple_map_message_handle_error)
            .inspect_err(log_server_error)
        })
        .await
}
