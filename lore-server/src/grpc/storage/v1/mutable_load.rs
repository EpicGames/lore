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
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::mutable_load::handle_mutable_load;
use crate::util::setup_execution;

#[tracing::instrument(name = "StorageServiceV1::MutableLoad", skip_all)]
pub async fn handler(
    request: Request<storage_v1::MutableLoadRequest>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<storage_v1::MutableLoadResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            let req = request.into_inner();

            let key = Hash::from(&req.key[..]);
            let key_type = KeyType::try_from(req.key_type).map_err(|_err| {
                Status::invalid_argument(format!("Invalid key_type: {}", req.key_type))
            })?;

            handle_mutable_load(
                key,
                key_type,
                repository,
                correlation_id,
                user_id,
                mutable_store,
            )
            .await
            .map(|resp| {
                let LoreResponse::MutableLoad(resp) = resp else {
                    panic!("MutableLoad handler returned the wrong response type");
                };
                Response::new(storage_v1::MutableLoadResponse {
                    value: bytes::Bytes::copy_from_slice(resp.value.as_ref()),
                })
            })
            .map_err(|e| match e {
                MessageHandleError::MutableDataNotFound(_) => {
                    Status::not_found("Mutable key not found")
                }
                other => simple_map_message_handle_error(other),
            })
            .inspect_err(log_server_error)
        })
        .await
}

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_proto::lore::storage::v1 as storage_v1;
    use lore_proto::lore::storage::v1::storage_service_server::StorageService as StorageServiceV1;
    use rand::random;
    use zerocopy::IntoBytes;

    use crate::grpc::storage::v1::test_utils::make_request_with_metadata;
    use crate::grpc::storage_service::LoreStorageService;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn test_v1_mutable_load_not_found() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        let repository = random::<Context>();
        let key = random::<Hash>();

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let service = LoreStorageService::new(
                    immutable_store.clone(),
                    immutable_store,
                    mutable_store,
                );

                let load_request = storage_v1::MutableLoadRequest {
                    key: bytes::Bytes::copy_from_slice(key.as_bytes()),
                    key_type: 0,
                };
                let request =
                    make_request_with_metadata(load_request, repository, "test-not-found");

                let result = StorageServiceV1::mutable_load(&service, request).await;
                assert!(result.is_err(), "Load of non-existent key should fail");
                assert_eq!(
                    result.unwrap_err().code(),
                    tonic::Code::NotFound,
                    "Should return NotFound"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
