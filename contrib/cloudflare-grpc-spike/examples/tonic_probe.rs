// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Diagnose the same TLS channel and unary protocol used by the current Lore client.
use std::{error::Error, time::Duration};

use tonic::{
    Request,
    client::Grpc,
    transport::{Channel, ClientTlsConfig},
};

#[derive(Clone, PartialEq, prost::Message)]
struct EnvironmentGetRequest {}

#[derive(Clone, PartialEq, prost::Message)]
struct EnvironmentGetResponse {
    #[prost(message, optional, tag = "1")]
    environment: Option<Environment>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Environment {}

#[derive(Clone, PartialEq, prost::Message)]
struct Address {
    #[prost(bytes, tag = "1")]
    hash: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ItemStatus {
    #[prost(int32, tag = "1")]
    code: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct GetResponse {
    #[prost(message, optional, tag = "1")]
    address: Option<Address>,
    #[prost(message, optional, tag = "4")]
    status: Option<ItemStatus>,
}

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: tonic_probe https://HOST");
    if let Err(err) = probe(url).await {
        eprintln!("probe failed: {err}");
        let mut source = err.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

async fn probe(url: String) -> Result<(), Box<dyn Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut endpoint = Channel::from_shared(url.clone())?;
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(
            ClientTlsConfig::new()
                .assume_http2(true)
                .with_webpki_roots()
                .with_native_roots(),
        )?;
    } else if let Ok(origin) = std::env::var("LORE_SPIKE_ORIGIN") {
        endpoint = endpoint.origin(origin.parse()?);
    }
    let channel = endpoint
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await?;
    println!("HTTP/2 connection established");
    let mut grpc = Grpc::new(channel);
    grpc.ready().await?;
    let path = http::uri::PathAndQuery::from_static(
        "/lore.environment.v1.EnvironmentService/EnvironmentGet",
    );
    let response: tonic::Response<EnvironmentGetResponse> = grpc
        .unary(
            Request::new(EnvironmentGetRequest {}),
            path,
            tonic_prost::ProstCodec::default(),
        )
        .await?;
    if response.get_ref().environment.is_none() {
        return Err("response has no environment".into());
    }
    println!("EnvironmentGet succeeded");

    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    sender.send(Address { hash: vec![1] }).await?;
    let path = http::uri::PathAndQuery::from_static("/lore.storage.v1.StorageService/Get");
    grpc.ready().await?;
    let mut replies: tonic::Streaming<GetResponse> = tokio::time::timeout(
        Duration::from_secs(10),
        grpc.streaming(
            Request::new(tokio_stream::wrappers::ReceiverStream::new(receiver)),
            path,
            tonic_prost::ProstCodec::default(),
        ),
    )
    .await??
    .into_inner();
    for hash in [1, 2] {
        if hash == 2 {
            sender.send(Address { hash: vec![hash] }).await?;
        }
        let reply = tokio::time::timeout(Duration::from_secs(10), replies.message())
            .await??
            .ok_or("response stream ended early")?;
        if reply.address.as_ref().map(|a| a.hash.as_slice()) != Some(&[hash][..])
            || reply.status.as_ref().map(|s| s.code) != Some(5)
        {
            return Err("unexpected Get response".into());
        }
        println!("Get item {hash} received before request closed");
    }
    drop(sender);
    match tokio::time::timeout(Duration::from_secs(10), replies.message()).await? {
        Ok(None) => println!("Get stream ended successfully"),
        Ok(Some(_)) => return Err("unexpected extra Get response".into()),
        Err(status) => return Err(status.into()),
    }
    Ok(())
}
