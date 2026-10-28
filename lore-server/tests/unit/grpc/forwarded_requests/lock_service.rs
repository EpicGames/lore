// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use lore_proto::lock::QueryRequest;
use lore_server::grpc::forwarded_requests::lock_service::*;
use tonic::Request;
use tonic::transport::Channel;

/// An unreachable peer must reach the caller as an `InternalClientError`
/// rather than as the peer's own answer, so the forwarding handler logs it
/// and substitutes a status of its own.
#[tokio::test]
async fn unreachable_peer_is_an_internal_client_error() {
    // Port 1 on loopback refuses immediately, and connect_lazy defers the
    // connection to the call so no peer has to exist to build the client.
    let uri = http::Uri::from_str("http://127.0.0.1:1/").expect("valid uri");
    let channel = Channel::builder(uri).connect_lazy();
    let mut client = GrpcForwardedLockServiceClient::new(channel);

    let request = Request::new(QueryRequest {
        branch: None,
        owner: Some("alice".into()),
        description: None,
    });

    let err = client
        .query(request)
        .await
        .expect_err("an unreachable peer is a client error");
    assert!(
        err.to_string().contains("did not reach the peer"),
        "unexpected error: {err}"
    );
}
