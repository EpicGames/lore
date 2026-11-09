// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Lore over a Unix domain socket: the `lore-unix://<upstream host>/<repository>` remote.
//!
//! One socket carries two protocols, told apart by the first bytes a connection sends:
//!
//! - The storage protocol: the `lore-storage/0.4` commands and 12-byte headers, written whole
//!   onto the socket after a handshake, with no streams, encryption or packetization. Requests
//!   complete in any order, matched to their responses by command id.
//! - gRPC over plaintext HTTP/2 for every other service. The request keeps the logical host in its
//!   authority, and the server forwards it to that host.
//!
//! `LORE_UDS_SOCKET` names the socket, and turns it on for the whole process: every gRPC channel
//! the process opens goes over it from the first, the authentication service's included, so a
//! client that authenticates over gRPC needs no network access of its own. OIDC requests are HTTP
//! and still go to the network. The process reaches over gRPC only the hosts the machine server
//! forwards, so a remote of another scheme works only if the server forwards its host and the
//! process has the network access its QUIC storage needs, over one connection. A `lore-unix`
//! remote without the variable set is refused.
pub mod client;
pub mod io;

use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::task::Context;
use std::task::Poll;

use hyper_util::rt::TokioIo;
use lore_credential::domain_from_url_str_or_url;
use lore_error_set::prelude::*;

use crate::auth::AuthEndpoint;
use crate::connection::SuppliedCredentials;
use crate::error::ProtocolError;
use crate::local_socket;
use crate::local_socket::AsyncStream;
use crate::traits::Storage;

/// Scheme of a remote reached through the machine server's socket.
pub const SCHEME: &str = "lore-unix";
/// Environment variable naming the socket every gRPC channel of the process goes through.
pub const SOCKET_ENV: &str = "LORE_UDS_SOCKET";

/// First bytes of a storage connection. An HTTP/2 connection starts with `PRI `.
pub const MAGIC: [u8; 8] = *b"LOREUDS1";
/// Version of the socket protocol this build speaks.
pub const VERSION: u16 = 1;
/// Client hello: magic (8), version u16, flags u16, reserved u32.
pub const CLIENT_HELLO_SIZE: usize = 16;
/// Server hello: magic (8), version u16, handler timeout in seconds u16, largest message body
/// u32, credits u32, credit bytes u32.
pub const SERVER_HELLO_SIZE: usize = 24;
/// Size of the header before every message body, as on QUIC.
pub const HEADER_SIZE: usize = crate::quic::command_header::COMMAND_HEADER_SIZE_V4;
/// Largest message body on the socket: a whole fragment and its command overhead, as on QUIC,
/// with room for the key `put_resolved` adds.
pub const MAX_MESSAGE: usize = crate::quic::storage_service::MAX_CHUNK_SIZE + 256;

/// What the server states in answer to a client hello.
#[derive(Clone, Copy, Debug)]
pub struct ServerHello {
    /// Version of the socket protocol the server speaks.
    pub version: u16,
    /// Longest the server handles one request before answering `SlowDown`; 0 if it does not say.
    pub handler_timeout_seconds: u16,
    /// Largest message body either side may send, header excluded.
    pub max_message: u32,
    /// Requests a client may have outstanding.
    pub credits: u32,
    /// Bytes a connection may hold: each request in handling by its body, or by the largest
    /// message where it is a read (`Get` or `GetResolved`), whose response it counts before it
    /// exists, and the responses not yet written. The server keeps to it by reading no further;
    /// the client does not enforce it.
    pub credit_bytes: u32,
}

/// This build's client hello: the magic and the version, with no flags or reserved bytes set.
pub fn client_hello() -> [u8; CLIENT_HELLO_SIZE] {
    let mut hello = [0u8; CLIENT_HELLO_SIZE];
    hello[..8].copy_from_slice(&MAGIC);
    hello[8..10].copy_from_slice(&VERSION.to_le_bytes());
    hello
}

/// The client's version, if `hello` is a client hello: the magic, the version, and no flags or
/// reserved bytes set, which no version defines yet.
pub fn parse_client_hello(hello: &[u8; CLIENT_HELLO_SIZE]) -> Option<u16> {
    (hello[..8] == MAGIC && hello[10..] == [0; 6]).then(|| u16::from_le_bytes([hello[8], hello[9]]))
}

impl ServerHello {
    /// The hello as the server writes it.
    pub fn to_bytes(self) -> [u8; SERVER_HELLO_SIZE] {
        let mut hello = [0u8; SERVER_HELLO_SIZE];
        hello[..8].copy_from_slice(&MAGIC);
        hello[8..10].copy_from_slice(&self.version.to_le_bytes());
        hello[10..12].copy_from_slice(&self.handler_timeout_seconds.to_le_bytes());
        hello[12..16].copy_from_slice(&self.max_message.to_le_bytes());
        hello[16..20].copy_from_slice(&self.credits.to_le_bytes());
        hello[20..24].copy_from_slice(&self.credit_bytes.to_le_bytes());
        hello
    }

    /// The server hello `hello` holds, if it starts with the magic.
    pub fn from_bytes(hello: &[u8; SERVER_HELLO_SIZE]) -> Option<Self> {
        if hello[..8] != MAGIC {
            return None;
        }
        let word = |at: usize| {
            u32::from_le_bytes([hello[at], hello[at + 1], hello[at + 2], hello[at + 3]])
        };
        Some(Self {
            version: u16::from_le_bytes([hello[8], hello[9]]),
            handler_timeout_seconds: u16::from_le_bytes([hello[10], hello[11]]),
            max_message: word(12),
            credits: word(16),
            credit_bytes: word(20),
        })
    }
}

static SOCKET: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    std::env::var_os(SOCKET_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
});

/// The socket every gRPC channel of this process goes through: the one `LORE_UDS_SOCKET` names.
pub fn route() -> Option<&'static Path> {
    SOCKET.as_deref()
}

/// The socket a `lore-unix` remote is reached through, refused when `LORE_UDS_SOCKET` names none.
pub fn socket() -> Result<&'static Path, ProtocolError> {
    route().ok_or_else(|| {
        ProtocolError::internal(format!(
            "a {SCHEME} remote is reached through the machine server's socket, which {SOCKET_ENV} must name"
        ))
    })
}

/// Spawns `future`, a connection's handshake or one of the tasks it runs for its lifetime, on the
/// net runtime with the rest of the process's socket I/O, outside any command's context: a socket
/// registers with the runtime it is opened on, and a connection outlives the command that happens
/// to open it.
pub fn spawn_io<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    lore_base::lore_spawn_net_nocontext!(future)
}

/// The URI a gRPC channel to `uri` names: in a process that names a socket, its plaintext form,
/// keeping the authority whose host the server forwards by, since TLS is the server's business on
/// the way out; else `uri` itself.
pub fn channel_uri(uri: String) -> String {
    if route().is_some()
        && let Some(rest) = uri.strip_prefix("https://")
    {
        return format!("http://{rest}");
    }
    uri
}

/// Connects `endpoint` through the socket the process names, or over the network where it names
/// none.
pub async fn connect_channel(
    endpoint: tonic::transport::Endpoint,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    match route() {
        Some(path) => {
            endpoint
                .connect_with_connector(SocketConnector::new(path))
                .await
        }
        None => endpoint.connect().await,
    }
}

/// Dials the socket for a tonic channel, whatever URI the channel names.
#[derive(Clone)]
pub struct SocketConnector {
    path: &'static Path,
}

impl SocketConnector {
    /// A connector that dials the socket at `path` for every connection a channel opens.
    pub fn new(path: &'static Path) -> Self {
        Self { path }
    }
}

impl tower::Service<http::Uri> for SocketConnector {
    type Response = TokioIo<ChannelStream>;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        let path = self.path;
        Box::pin(async move {
            Ok(TokioIo::new(ChannelStream(
                local_socket::connect(path).await?,
            )))
        })
    }
}

/// The socket a tonic channel runs over. Its vectored writes, as hyper makes them, fail on a peer
/// that has gone rather than raise `SIGPIPE`, as [`local_socket::try_send_vectored`] does. Its
/// plain writes pass `MSG_NOSIGNAL` already.
pub struct ChannelStream(AsyncStream);

impl tokio::io::AsyncRead for ChannelStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ChannelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let stream = &self.get_mut().0;
        loop {
            std::task::ready!(stream.poll_write_ready(cx))?;
            match local_socket::try_send_vectored(stream, buffers) {
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                written => return Poll::Ready(written),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Storage over the socket.
pub async fn storage(
    remote_url: &str,
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    credentials: &Arc<SuppliedCredentials>,
) -> Result<Arc<dyn Storage>, ProtocolError> {
    let remote_domain = domain_from_url_str_or_url(remote_url)
        .internal_with(|| format!("remote {remote_url} is invalid"))?;
    let client = client::UdsStorageClient::connect(
        socket()?.to_path_buf(),
        remote_domain,
        auth_endpoint,
        identity,
        credentials,
    )
    .await?;
    Ok(client)
}
