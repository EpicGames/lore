// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use async_trait::async_trait;
use lore_proto::lore::repository::v1;
use tonic::Request;
use tonic::transport::Channel;

use crate::grpc::forwarded_requests::ForwardedRequestResult;
use crate::grpc::forwarded_requests::classify_forwarded_result;

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
        classify_forwarded_result(self.client.repository_create(request).await)
    }

    async fn repository_get(
        &mut self,
        request: Request<v1::RepositoryGetRequest>,
    ) -> ForwardedRequestResult<v1::RepositoryGetResponse> {
        classify_forwarded_result(self.client.repository_get(request).await)
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use super::*;

    /// An unreachable peer must reach the caller as an `InternalClientError`
    /// rather than as the peer's own answer, so the forwarding handlers log it
    /// and substitute a status of their own.
    #[tokio::test]
    async fn unreachable_peer_is_an_internal_client_error() {
        // Port 1 on loopback refuses immediately, and connect_lazy defers the
        // connection to the call so no peer has to exist to build the client.
        let uri = http::Uri::from_str("http://127.0.0.1:1/").expect("valid uri");
        let channel = Channel::builder(uri).connect_lazy();
        let mut client = GrpcForwardedRepositoryServiceClient::new(channel);

        let request = Request::new(v1::RepositoryGetRequest {
            query: Some(v1::repository_get_request::Query::Name("my-repo".into())),
        });

        let err = client
            .repository_get(request)
            .await
            .expect_err("an unreachable peer is a client error");
        assert!(
            err.to_string().contains("did not reach the peer"),
            "unexpected error: {err}"
        );
    }
}
