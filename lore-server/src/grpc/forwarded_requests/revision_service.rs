// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use async_trait::async_trait;
use lore_proto::lore::revision::v1;
use tonic::Request;
use tonic::transport::Channel;

use crate::grpc::forwarded_requests::ForwardedRequestResult;
use crate::grpc::revision::v1::branch_list::BranchListStream;

#[async_trait]
pub trait ForwardedRevisionServiceClient: Send + Sync {
    async fn branch_create(
        &mut self,
        request: Request<v1::BranchCreateRequest>,
    ) -> ForwardedRequestResult<v1::BranchCreateResponse>;

    async fn branch_delete(
        &mut self,
        request: Request<v1::BranchDeleteRequest>,
    ) -> ForwardedRequestResult<v1::BranchDeleteResponse>;

    async fn branch_get(
        &mut self,
        request: Request<v1::BranchGetRequest>,
    ) -> ForwardedRequestResult<v1::BranchGetResponse>;

    async fn branch_list(
        &mut self,
        request: Request<v1::BranchListRequest>,
    ) -> ForwardedRequestResult<BranchListStream>;
}

pub struct GrpcForwardedRevisionServiceClient {
    client: v1::forwarded_revision_service_client::ForwardedRevisionServiceClient<Channel>,
}

impl GrpcForwardedRevisionServiceClient {
    pub fn new(channel: Channel) -> Self {
        let client =
            v1::forwarded_revision_service_client::ForwardedRevisionServiceClient::new(channel);
        Self { client }
    }
}

#[async_trait]
impl ForwardedRevisionServiceClient for GrpcForwardedRevisionServiceClient {
    async fn branch_create(
        &mut self,
        request: Request<v1::BranchCreateRequest>,
    ) -> ForwardedRequestResult<v1::BranchCreateResponse> {
        Ok(self.client.branch_create(request).await)
    }

    async fn branch_delete(
        &mut self,
        request: Request<v1::BranchDeleteRequest>,
    ) -> ForwardedRequestResult<v1::BranchDeleteResponse> {
        Ok(self.client.branch_delete(request).await)
    }

    async fn branch_get(
        &mut self,
        request: Request<v1::BranchGetRequest>,
    ) -> ForwardedRequestResult<v1::BranchGetResponse> {
        Ok(self.client.branch_get(request).await)
    }

    async fn branch_list(
        &mut self,
        request: Request<v1::BranchListRequest>,
    ) -> ForwardedRequestResult<BranchListStream> {
        // The gRPC client returns Response<tonic::Streaming<T>>; box-pin the
        // inner Streaming so the response carries the BranchListStream type
        // alias expected by the rest of the forwarding path.
        let result: Result<tonic::Response<_>, _> = self.client.branch_list(request).await;
        Ok(result.map(|resp| {
            let streaming = resp.into_inner();
            tonic::Response::new(Box::pin(streaming) as BranchListStream)
        }))
    }
}
