// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A loopback QUIC server speaking just enough of lore-storage/0.4 for client tests: session
//! start and stop, the client's user agent, and `get` of any address on a live session.
//!
//! Each connection serves one `get` at a time. A gathering server holds each until every
//! connection has one in flight, so its `get`s finish only when they are spread over all of them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_transport::connection::SuppliedCredentials;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::quic::storage_service::client::StorageClient;
use lore_transport::traits::Storage;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use zerocopy::IntoBytes;

/// Payload bytes the server returns for every `get`.
pub(crate) const PAYLOAD_SIZE: usize = 1024;

/// The partition test sessions are started for.
pub(crate) fn test_partition() -> Partition {
    Partition::from([0x3cu8; 16])
}

/// A distinct address per `index`; the server answers any.
pub(crate) fn test_address(index: usize) -> Address {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    Address {
        hash: Hash::from(hash),
        context: Context::default(),
    }
}

pub(crate) struct TestStorageServer {
    pub url: String,
    gets: Arc<parking_lot::Mutex<Vec<Arc<AtomicU64>>>>,
    sessions: Arc<parking_lot::Mutex<Vec<Arc<parking_lot::Mutex<SessionMap>>>>>,
    endpoint: quinn::Endpoint,
}

/// The sessions one connection has started and not stopped, numbered from 1 as a server's
/// session map numbers them.
struct SessionMap {
    next: u32,
    live: Vec<u32>,
}

impl Default for SessionMap {
    fn default() -> Self {
        Self {
            next: 1,
            live: Vec::new(),
        }
    }
}

impl Drop for TestStorageServer {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"test over");
    }
}

impl TestStorageServer {
    /// Serves on a loopback port, answering each `get` at once.
    pub fn start() -> Self {
        Self::serve(None)
    }

    /// Serves on a loopback port, holding each `get` until each of `connections` connections has
    /// one in flight.
    pub fn start_gathering(connections: usize) -> Self {
        Self::serve(Some(Arc::new(tokio::sync::Barrier::new(connections))))
    }

    fn serve(gathering: Option<Arc<tokio::sync::Barrier>>) -> Self {
        let certificate = lore_transport::tls::generate_self_signed(vec!["127.0.0.1".to_string()])
            .expect("self-signed certificate");
        let chain = vec![
            CertificateDer::from_pem_slice(certificate.cert_pem.as_bytes()).expect("certificate"),
        ];
        let key = PrivateKeyDer::from_pem_slice(certificate.key_pem.as_bytes()).expect("key");
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("server certificate");
        tls.alpn_protocols = vec![b"lore-storage/0.4".to_vec()];
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(tls).expect("QUIC server crypto"),
        ));
        Arc::get_mut(&mut server_config.transport)
            .expect("fresh transport config")
            .max_concurrent_bidi_streams(lore_transport::quic::client::STREAM_COUNT.into());

        let address: SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
        let endpoint = quinn::Endpoint::server(server_config, address).expect("server endpoint");
        let port = endpoint.local_addr().expect("bound address").port();
        let gets: Arc<parking_lot::Mutex<Vec<Arc<AtomicU64>>>> = Arc::default();
        let sessions: Arc<parking_lot::Mutex<Vec<Arc<parking_lot::Mutex<SessionMap>>>>> =
            Arc::default();

        let accepting = endpoint.clone();
        let (accepted, accepted_sessions) = (gets.clone(), sessions.clone());
        lore_spawn!(async move {
            while let Some(incoming) = accepting.accept().await {
                let Ok(connection) = incoming.await else {
                    continue;
                };
                let count = Arc::new(AtomicU64::new(0));
                accepted.lock().push(count.clone());
                let map = Arc::new(parking_lot::Mutex::new(SessionMap::default()));
                accepted_sessions.lock().push(map.clone());
                lore_spawn!(serve_connection(connection, count, map, gathering.clone()));
            }
        });

        Self {
            url: format!("lore://127.0.0.1:{port}"),
            gets,
            sessions,
            endpoint,
        }
    }

    /// Forgets every connection's sessions and numbers new ones from 1 again, as the fresh
    /// session map of a reconnected connection does.
    pub fn restart_sessions(&self) {
        for map in self.sessions.lock().iter() {
            *map.lock() = SessionMap::default();
        }
    }

    /// `get` requests served so far, per connection in the order they were accepted.
    pub fn gets_per_connection(&self) -> Vec<u64> {
        self.gets
            .lock()
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .collect()
    }

    /// Opens `count` storage connections.
    pub async fn connect(&self, count: usize) -> Vec<Arc<dyn Storage>> {
        let mut connected: Vec<Arc<dyn Storage>> = Vec::with_capacity(count);
        for _ in 0..count {
            let client = StorageClient::connect(
                Weak::new(),
                &self.url,
                "127.0.0.1".to_string(),
                "",
                "",
                test_partition(),
                &Arc::new(SuppliedCredentials::default()),
                None,
            )
            .await
            .expect("connect to the test server");
            connected.push(Arc::new(client));
        }
        connected
    }

    /// Opens `count` storage connections, each with a session started for [`test_partition`].
    pub async fn connect_with_sessions(&self, count: usize) -> Vec<(Arc<dyn Storage>, u32)> {
        let mut connected = Vec::with_capacity(count);
        for storage in self.connect(count).await {
            let session_id = storage
                .session_start(test_partition(), "test")
                .await
                .expect("session start");
            connected.push((storage, session_id));
        }
        connected
    }
}

async fn serve_connection(
    connection: quinn::Connection,
    gets: Arc<AtomicU64>,
    sessions: Arc<parking_lot::Mutex<SessionMap>>,
    gathering: Option<Arc<tokio::sync::Barrier>>,
) {
    let serial = Arc::new(tokio::sync::Mutex::new(()));
    while let Ok((send, recv)) = connection.accept_bi().await {
        lore_spawn!(serve_stream(
            send,
            recv,
            gets.clone(),
            serial.clone(),
            sessions.clone(),
            gathering.clone(),
        ));
    }
}

async fn serve_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    gets: Arc<AtomicU64>,
    serial: Arc<tokio::sync::Mutex<()>>,
    sessions: Arc<parking_lot::Mutex<SessionMap>>,
    gathering: Option<Arc<tokio::sync::Barrier>>,
) {
    let mut header_bytes = [0u8; COMMAND_HEADER_SIZE_V4];
    while recv.read_exact(&mut header_bytes).await.is_ok() {
        let header = CommandHeader::from_bytes_v4(&header_bytes);
        let mut request = vec![0u8; header.size_or_status as usize];
        if recv.read_exact(&mut request).await.is_err() {
            return;
        }
        let response = match header.cmd {
            command if command == Command::Authorize as u8 && request.first() == Some(&0) => {
                let mut map = sessions.lock();
                let session_id = map.next;
                map.next += 1;
                map.live.push(session_id);
                Ok(Bytes::copy_from_slice(&session_id.to_le_bytes()))
            }
            command if command == Command::Authorize as u8 && request.first() == Some(&1) => {
                sessions
                    .lock()
                    .live
                    .retain(|&live| live != header.session_id);
                Ok(Bytes::new())
            }
            command if command == Command::Get as u8 => {
                serve_get(header.session_id, &gets, &serial, &sessions, &gathering).await
            }
            _ => Ok(Bytes::new()),
        };
        let (response_header, length) = match &response {
            Ok(response) => header.response_success(response.len() as u32),
            Err(status) => header.response_error(*status),
        }
        .response_bytes();
        let mut chunks = [
            Bytes::copy_from_slice(&response_header[..length]),
            response.unwrap_or_default(),
        ];
        if send.write_all_chunks(&mut chunks).await.is_err() {
            return;
        }
    }
}

/// A `get` on `session_id`, served one at a time per connection and held as the server gathers,
/// or `Failed` where the connection holds no such session.
async fn serve_get(
    session_id: u32,
    gets: &AtomicU64,
    serial: &tokio::sync::Mutex<()>,
    sessions: &parking_lot::Mutex<SessionMap>,
    gathering: &Option<Arc<tokio::sync::Barrier>>,
) -> Result<Bytes, u32> {
    if !sessions.lock().live.contains(&session_id) {
        return Err(QuicServiceError::Failed as u32);
    }
    {
        let _serial = serial.lock().await;
        if let Some(gathering) = gathering {
            gathering.wait().await;
        }
    }
    gets.fetch_add(1, Ordering::Relaxed);
    let fragment = Fragment {
        flags: 0,
        size_payload: PAYLOAD_SIZE as u32,
        size_content: PAYLOAD_SIZE as u64,
    };
    let mut response = fragment.as_bytes().to_vec();
    response.resize(response.len() + PAYLOAD_SIZE, 0xab);
    Ok(Bytes::from(response))
}
