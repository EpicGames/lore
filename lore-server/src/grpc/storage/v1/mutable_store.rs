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
use crate::protocol::storage::mutable_store_handler::handle_mutable_store;
use crate::util::setup_execution;

#[tracing::instrument(name = "StorageServiceV1::MutableStore", skip_all)]
pub async fn handler(
    request: Request<storage_v1::MutableStoreRequest>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<storage_v1::MutableStoreResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            let req = request.into_inner();

            let key = Hash::from(&req.key[..]);
            let value = Hash::from(&req.value[..]);
            let key_type = KeyType::try_from(req.key_type).map_err(|_err| {
                Status::invalid_argument(format!("Invalid key_type: {}", req.key_type))
            })?;

            handle_mutable_store(
                key,
                value,
                key_type,
                repository,
                correlation_id,
                user_id,
                mutable_store,
            )
            .await
            .map(|_| Response::new(storage_v1::MutableStoreResponse {}))
            .map_err(simple_map_message_handle_error)
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
    use lore_proto::lore::storage::v1::storage_service_server::StorageService as StorageServiceV1;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::grpc::storage::v1::test_utils::make_request_with_metadata;
    use crate::grpc::storage_service::LoreStorageService;
    use crate::store::test_store_create;

    /// Round-trip test that exercises `MutableStore` → `MutableLoad` → `MutableCompareAndSwap` → `MutableLoad` against the gRPC v1 service.
    #[tokio::test]
    async fn test_v1_mutable_store_load_cas() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        let repository = random::<Context>();
        let key = random::<Hash>();
        let value = random::<Hash>();
        let new_value = random::<Hash>();

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let service = LoreStorageService::new(
                    immutable_store.clone(),
                    immutable_store,
                    mutable_store,
                );

                let store_request = storage_v1::MutableStoreRequest {
                    key: bytes::Bytes::copy_from_slice(key.as_bytes()),
                    value: bytes::Bytes::copy_from_slice(value.as_bytes()),
                    key_type: 0,
                };
                let request =
                    make_request_with_metadata(store_request, repository, "test-mutable-corr");

                StorageServiceV1::mutable_store(&service, request)
                    .await
                    .expect("MutableStore should succeed");

                let load_request = storage_v1::MutableLoadRequest {
                    key: bytes::Bytes::copy_from_slice(key.as_bytes()),
                    key_type: 0,
                };
                let request =
                    make_request_with_metadata(load_request, repository, "test-mutable-corr");

                let load_response = StorageServiceV1::mutable_load(&service, request)
                    .await
                    .expect("MutableLoad should succeed");

                let loaded_value = load_response.into_inner().value;
                assert_eq!(
                    loaded_value.as_ref(),
                    value.as_bytes(),
                    "Loaded value should match stored value"
                );

                let cas_request = storage_v1::MutableCompareAndSwapRequest {
                    key: bytes::Bytes::copy_from_slice(key.as_bytes()),
                    expected: bytes::Bytes::copy_from_slice(value.as_bytes()),
                    value: bytes::Bytes::copy_from_slice(new_value.as_bytes()),
                    key_type: 0,
                };
                let request =
                    make_request_with_metadata(cas_request, repository, "test-mutable-corr");

                let cas_response = StorageServiceV1::mutable_compare_and_swap(&service, request)
                    .await
                    .expect("MutableCas should succeed");

                let current = cas_response.into_inner().current_value;
                // CAS returns the value after the swap (the new value if it succeeded)
                // or the actual current value if the expected didn't match.
                // The specific behavior depends on the store implementation.
                // Just verify we get a valid 32-byte hash back.
                assert_eq!(current.len(), 32, "CAS should return a 32-byte hash");

                let load_request = storage_v1::MutableLoadRequest {
                    key: bytes::Bytes::copy_from_slice(key.as_bytes()),
                    key_type: 0,
                };
                let request =
                    make_request_with_metadata(load_request, repository, "test-mutable-corr");

                let load_response = StorageServiceV1::mutable_load(&service, request)
                    .await
                    .expect("MutableLoad after CAS should succeed");

                let loaded_value = load_response.into_inner().value;
                assert_eq!(
                    loaded_value.as_ref(),
                    new_value.as_bytes(),
                    "Value should be updated after CAS"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
