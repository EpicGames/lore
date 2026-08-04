// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_proto::lore::storage::v1 as storage_v1;
use lore_proto::lore::storage::v1::storage_service_server::StorageService as StorageServiceV1;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;

use super::copy;
use super::copy::CopyResponseStream;
use super::get;
use super::get::GetResponseStream;
use super::get_metadata;
use super::mutable_compare_and_swap;
use super::mutable_load;
use super::mutable_store;
use super::presign_download;
use super::put;
use super::put::PutResponseStream;
use super::query;
use super::upload_content;
use super::verify;
use crate::grpc::storage_service::LoreStorageService;

#[tonic::async_trait]
impl StorageServiceV1 for LoreStorageService {
    type GetStream = GetResponseStream;

    async fn get(
        &self,
        request: Request<Streaming<lore_proto::lore::model::v1::Address>>,
    ) -> Result<Response<Self::GetStream>, Status> {
        get::handler(request, self.immutable_store().clone(), self).await
    }

    type GetMetadataStream = GetResponseStream;

    async fn get_metadata(
        &self,
        request: Request<Streaming<lore_proto::lore::model::v1::Address>>,
    ) -> Result<Response<Self::GetMetadataStream>, Status> {
        get_metadata::handler(request, self.immutable_store().clone(), self).await
    }

    type PutStream = PutResponseStream;

    async fn put(
        &self,
        request: Request<Streaming<storage_v1::PutRequest>>,
    ) -> Result<Response<Self::PutStream>, Status> {
        put::handler(request, self.immutable_store().clone(), self).await
    }

    async fn upload_content(
        &self,
        request: Request<Streaming<storage_v1::UploadContentRequest>>,
    ) -> Result<Response<storage_v1::UploadContentResponse>, Status> {
        upload_content::handler(
            request,
            self.immutable_store().clone(),
            self.upload_content_max_bytes(),
        )
        .await
    }

    async fn query(
        &self,
        request: Request<storage_v1::QueryRequest>,
    ) -> Result<Response<storage_v1::QueryResponse>, Status> {
        query::handler(request, self.immutable_store().clone()).await
    }

    async fn presign_download(
        &self,
        request: Request<storage_v1::PresignDownloadRequest>,
    ) -> Result<Response<storage_v1::PresignDownloadResponse>, Status> {
        presign_download::handler(request, self.immutable_store().clone()).await
    }

    type CopyStream = CopyResponseStream;

    async fn copy(
        &self,
        request: Request<Streaming<storage_v1::CopyRequest>>,
    ) -> Result<Response<Self::CopyStream>, Status> {
        copy::handler(request, self.immutable_store().clone(), self).await
    }

    async fn verify(
        &self,
        request: Request<storage_v1::VerifyRequest>,
    ) -> Result<Response<storage_v1::VerifyResponse>, Status> {
        verify::handler(request, self.local_immutable_store().clone()).await
    }

    async fn mutable_load(
        &self,
        request: Request<storage_v1::MutableLoadRequest>,
    ) -> Result<Response<storage_v1::MutableLoadResponse>, Status> {
        mutable_load::handler(request, self.mutable_store().clone()).await
    }

    async fn mutable_store(
        &self,
        request: Request<storage_v1::MutableStoreRequest>,
    ) -> Result<Response<storage_v1::MutableStoreResponse>, Status> {
        mutable_store::handler(request, self.mutable_store().clone()).await
    }

    async fn mutable_compare_and_swap(
        &self,
        request: Request<storage_v1::MutableCompareAndSwapRequest>,
    ) -> Result<Response<storage_v1::MutableCompareAndSwapResponse>, Status> {
        mutable_compare_and_swap::handler(request, self.mutable_store().clone()).await
    }
}

#[cfg(test)]
mod tests {
    use lore_proto::lore::storage::v1::storage_service_server::StorageServiceServer;

    use crate::grpc::storage_service::LoreStorageService;

    /// Compile-time check that `LoreStorageService` fully implements the generated
    /// `StorageService` trait — wrapping it in `StorageServiceServer` requires the
    /// trait bound to hold. Per-handler behavior is tested in each handler module.
    #[allow(dead_code)]
    fn assert_implements_trait(
        service: LoreStorageService,
    ) -> StorageServiceServer<LoreStorageService> {
        StorageServiceServer::new(service)
    }
}
