// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use async_trait::async_trait;
use lore_proto::lore::repository::v1;
use tonic::Request;
use tonic::transport::Channel;

use crate::grpc::forwarded_requests::ForwardedRequestResult;

#[async_trait]
pub trait ForwardedRepositoryServiceClient: Send + Sync {
    async fn repository_create(
        &mut self,
        request: Request<v1::RepositoryCreateRequest>,
    ) -> ForwardedRequestResult<v1::RepositoryCreateResponse>;

    async fn repository_get(
        &mut self,
        request: Request<v1::RepositoryGetRequest>,
    ) -> ForwardedRequestResult<v1::RepositoryGetResponse>;
}

pub struct GrpcForwardedRepositoryServiceClient {
    client: v1::forwarded_repository_service_client::ForwardedRepositoryServiceClient<Channel>,
}

impl GrpcForwardedRepositoryServiceClient {
    pub fn new(channel: Channel) -> Self {
        let client =
            v1::forwarded_repository_service_client::ForwardedRepositoryServiceClient::new(channel);
        Self { client }
    }
}

#[async_trait]
impl ForwardedRepositoryServiceClient for GrpcForwardedRepositoryServiceClient {
    async fn repository_create(
        &mut self,
        request: Request<v1::RepositoryCreateRequest>,
    ) -> ForwardedRequestResult<v1::RepositoryCreateResponse> {
        Ok(self.client.repository_create(request).await)
    }

    async fn repository_get(
        &mut self,
        request: Request<v1::RepositoryGetRequest>,
    ) -> ForwardedRequestResult<v1::RepositoryGetResponse> {
        Ok(self.client.repository_get(request).await)
    }
}
