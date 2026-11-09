// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! gRPC from a container's socket, forwarded to the upstream host the request names.
//!
//! Requests arrive as plaintext HTTP/2 whose authority is the logical host the client dialed: the
//! upstream Lore server, its authentication service or its notification service. Only hosts in
//! `[server.uds.forward]` are reached, compared without regard to case. Each socket reaches each
//! host over a channel of its own, connected at its first request, with TLS for an `https` target,
//! so a container that stops reading a response holds up no other container's calls. The caller's
//! metadata, its token included, travels unchanged, so the upstream authorizes the caller as it
//! would a direct client.
use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use anyhow::Context as _;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::interface::ExecutionContext;
use lore_transport::local_socket::AsyncStream;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tonic::Status;
use tonic::transport::Channel;
use tonic::transport::ClientTlsConfig;
use tonic::transport::Endpoint;
use tower::Service;
use tracing::debug;

use crate::grpc::warn_error_to_status;

/// The forward targets, each checked and configured once, from which each socket's forwarder
/// takes channels of its own.
pub struct ForwardTargets {
    /// The endpoint of each forwarded host's target, keyed by the host in lowercase.
    endpoints: HashMap<String, Endpoint>,
    executor: ContextExecutor,
}

/// One socket's forwarder.
pub struct GrpcProxy {
    /// The channel to each forwarded host's target, keyed by the host in lowercase.
    channels: HashMap<String, Channel>,
    executor: ContextExecutor,
}

/// Spawns the HTTP/2 connection's tasks on the net runtime under the forwarder's context.
#[derive(Clone)]
struct ContextExecutor(Arc<ExecutionContext>);

impl<F> hyper::rt::Executor<F> for ContextExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        lore_base::lore_spawn_net!(LORE_CONTEXT.scope(self.0.clone(), future));
    }
}

/// The configured host `host` names in `routes`, whose hosts are lowercase, and what it maps to.
/// Only a host with an uppercase letter pays for a lowercase copy.
#[lore_macro::test_pub]
fn route<'a, T>(routes: &'a HashMap<String, T>, host: &str) -> Option<(&'a String, &'a T)> {
    if host.bytes().any(|byte| byte.is_ascii_uppercase()) {
        routes.get_key_value(host.to_ascii_lowercase().as_str())
    } else {
        routes.get_key_value(host)
    }
}

impl ForwardTargets {
    /// The targets `routes` names by host, their tasks run under `execution`. A target that is no
    /// valid URL fails here.
    pub fn new(
        routes: HashMap<String, String>,
        execution: Arc<ExecutionContext>,
    ) -> anyhow::Result<Self> {
        let executor = ContextExecutor(execution);
        let endpoints = routes
            .into_iter()
            .map(|(host, target)| {
                let endpoint = endpoint(&target, executor.clone())
                    .with_context(|| format!("forwarding {host} to {target}"))?;
                Ok((host.to_ascii_lowercase(), endpoint))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            endpoints,
            executor,
        })
    }

    /// A forwarder for one socket, with a channel of its own to each target, connected at its
    /// first request.
    pub fn forwarder(&self) -> GrpcProxy {
        GrpcProxy {
            channels: self
                .endpoints
                .iter()
                .map(|(host, endpoint)| (host.clone(), endpoint.connect_lazy()))
                .collect(),
            executor: self.executor.clone(),
        }
    }
}

impl GrpcProxy {
    pub async fn serve(self: Arc<Self>, io: Prefixed) {
        let mut builder = hyper::server::conn::http2::Builder::new(self.executor.clone());
        let service = hyper::service::service_fn(move |request| {
            let proxy = self.clone();
            async move { Ok::<_, Infallible>(proxy.forward(request).await) }
        });
        builder.adaptive_window(true);
        if let Err(err) = builder.serve_connection(TokioIo::new(io), service).await {
            debug!(error = %err, "Forwarding connection ended");
        }
    }

    async fn forward(&self, request: http::Request<Incoming>) -> http::Response<tonic::body::Body> {
        let host = request.uri().host().unwrap_or_default();
        let Some((host, channel)) = route(&self.channels, host) else {
            return Status::permission_denied(format!("{host} is not forwarded")).into_http();
        };
        let mut channel = channel.clone();
        let request = request.map(tonic::body::Body::new);
        let response = match std::future::poll_fn(|cx| channel.poll_ready(cx)).await {
            Ok(()) => channel.call(request).await,
            Err(err) => Err(err),
        };
        match response {
            Ok(response) => response,
            Err(err) => warn_error_to_status(&err, |err| {
                Status::unavailable(format!("forwarding to {host}: {err}"))
            })
            .into_http(),
        }
    }
}

/// The endpoint of `target`, whose channels spawn their tasks with `executor`.
fn endpoint(target: &str, executor: ContextExecutor) -> anyhow::Result<Endpoint> {
    let mut endpoint = Endpoint::from_shared(target.to_string())?
        .executor(executor)
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_while_idle(true);
    if target.starts_with("https://") {
        endpoint = endpoint.tls_config(
            ClientTlsConfig::new()
                .assume_http2(true)
                .with_webpki_roots()
                .with_native_roots(),
        )?;
    }
    Ok(endpoint)
}

/// The client connection preface every HTTP/2 connection opens with (RFC 9113, section 3.4).
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// A socket whose HTTP/2 client preface was read and checked, replayed ahead of the rest.
pub struct Prefixed {
    offset: usize,
    inner: AsyncStream,
}

impl Prefixed {
    pub fn new(inner: AsyncStream) -> Self {
        Self { offset: 0, inner }
    }
}

impl AsyncRead for Prefixed {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.offset < PREFACE.len() {
            let count = (PREFACE.len() - this.offset).min(buf.remaining());
            buf.put_slice(&PREFACE[this.offset..this.offset + count]);
            this.offset += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
