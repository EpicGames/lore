// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment::generate_random;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::grpc::storage_service::LoreStorageService;
use lore_server::legacy::rpc::storage_service_client::StorageServiceClient;
use lore_server::legacy::rpc::storage_service_server::StorageServiceServer;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_transport::grpc::PARTITION_ID_KEY;
use rand::random;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::Request;
use tonic::Status;
use tonic::metadata::BinaryMetadataValue;
use tonic::transport::Channel;

use crate::store::test_support::SlowDownImmutableStore;
use crate::store::test_support::test_store_create;

/// Serves the legacy storage service over a store that answers every operation with a slow-down,
/// and returns a client connected to it.
async fn throttled_service_client(
    mutable_store: Arc<dyn MutableStore>,
) -> StorageServiceClient<Channel> {
    let store: Arc<dyn ImmutableStore> = Arc::new(SlowDownImmutableStore);
    let service = LoreStorageService::new(
        store.clone(),
        store,
        mutable_store,
        Arc::new(AllowAllRepositoryAuthorizer),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let address = listener.local_addr().expect("a bound address");
    lore_base::lore_spawn!(async move {
        tonic::transport::Server::builder()
            .add_service(StorageServiceServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    StorageServiceClient::connect(format!("http://{address}"))
        .await
        .expect("connect to the local server")
}

fn streaming_request<T: Send + 'static>(
    repository: Context,
    item: T,
) -> Request<tokio_stream::Iter<std::vec::IntoIter<T>>> {
    let mut request = Request::new(tokio_stream::iter(vec![item]));
    request.metadata_mut().insert_bin(
        PARTITION_ID_KEY,
        BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

/// Returns the status the first response item fails with.
async fn first_item_error<T: std::fmt::Debug>(
    response: Result<tonic::Response<tonic::Streaming<T>>, Status>,
) -> Status {
    response
        .expect("the stream opens")
        .into_inner()
        .message()
        .await
        .expect_err("the item fails")
}

#[tokio::test]
async fn throttled_get_reports_resource_exhausted() {
    let (_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let mut client = throttled_service_client(mutable_store).await;
            let (_fragment, address, _payload) = generate_random();
            let request = streaming_request(random::<Context>(), address.into());
            let status = first_item_error(client.get(request).await).await;
            assert_eq!(status.code(), Code::ResourceExhausted);
        })
        .await;
}

#[tokio::test]
async fn throttled_put_reports_resource_exhausted() {
    let (_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let mut client = throttled_service_client(mutable_store).await;
            let (fragment, address, payload) = generate_random();
            let request = streaming_request(
                random::<Context>(),
                lore_proto::model::PutRequest {
                    address: Some(address.into()),
                    fragment: Some(fragment.into()),
                    payload: Some(payload),
                },
            );
            let status = first_item_error(client.put(request).await).await;
            assert_eq!(status.code(), Code::ResourceExhausted);
        })
        .await;
}

#[tokio::test]
async fn throttled_copy_reports_resource_exhausted() {
    let (_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let mut client = throttled_service_client(mutable_store).await;
            let (_fragment, address, _payload) = generate_random();
            let repository = random::<Context>();
            let request = streaming_request(
                repository,
                lore_proto::model::CopyRequest {
                    source_repository_id: repository.data().to_vec().into(),
                    source_address: Some(address.into()),
                    target_context: Default::default(),
                },
            );
            let status = first_item_error(client.copy(request).await).await;
            assert_eq!(status.code(), Code::ResourceExhausted);
        })
        .await;
}
