// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A loopback QUIC server speaking just enough of lore-storage/0.4 for client tests: session
//! start and stop, the client's user agent, and `get` of any address on a live session, recording
//! the authorization each session start presents and the partition of the session each `get` ran
//! under. Every other command is answered with an empty success. A test can fault the stream that
//! answers a request ([`StreamFault`]).
//!
//! Each connection serves one `get` at a time. How the server answers is its [`Answering`]. Its
//! reading of request payloads can be paused, so that a client's large write is held.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_transport::auth::AuthEndpoint;
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

/// How far a client may write ahead of what the server has read on one stream: small, so that a
/// large request whose payload the server is not reading holds the client's write.
const STREAM_WINDOW: u32 = 16 * 1024;

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

/// Waits until `condition` holds, failing with `failure` after ten seconds.
pub(crate) async fn wait_until(condition: impl Fn() -> bool, failure: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect(failure);
}

pub(crate) struct TestStorageServer {
    pub url: String,
    gets: Arc<parking_lot::Mutex<Vec<Arc<AtomicU64>>>>,
    sessions: Arc<parking_lot::Mutex<Vec<Arc<parking_lot::Mutex<SessionMap>>>>>,
    presented: Arc<parking_lot::Mutex<Vec<String>>>,
    reading: tokio::sync::watch::Sender<bool>,
    fault: Arc<parking_lot::Mutex<Option<StreamFault>>>,
    closed: Arc<AtomicUsize>,
    endpoint: quinn::Endpoint,
}

/// What the server does to the stream serving its next request once armed with
/// [`TestStorageServer::arm_fault`], keeping the stream's connection up.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StreamFault {
    /// Stops reading the stream ahead of its answer, so the client has seen the stop by the time
    /// it reads the answer, and keeps its own side open: the client's writes to the stream fail
    /// while its reader carries on.
    StopReading,
    /// Answers, then answers a command the client never sent, and keeps the stream open: the
    /// client's reader stops while its writes to the stream still succeed.
    StrayAnswer,
}

/// The sessions one connection has started and not stopped, numbered from 1 as a server's
/// session map numbers them, each with its partition, the session starts it was asked for, the
/// `get`s it received, and the partition of the session each `get` ran under.
struct SessionMap {
    next: u32,
    live: Vec<(u32, Partition)>,
    requested: u32,
    received: u64,
    served: Vec<Partition>,
}

impl Default for SessionMap {
    fn default() -> Self {
        Self {
            next: 1,
            live: Vec::new(),
            requested: 0,
            received: 0,
            served: Vec::new(),
        }
    }
}

/// How a server a test controls answers a session start.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SessionStarts {
    Accept,
    /// With `SlowDown`.
    Refuse,
    /// With `NotAuthorized`, as a server does once the authorization presented is revoked.
    RefuseAuthorization,
    /// Answers none until the test says how to.
    Hold,
}

/// How a server answers its requests.
#[derive(Clone)]
enum Answering {
    /// Every request at once.
    AtOnce,
    /// Every `get` once each of a number of connections has one in flight, so its `get`s finish
    /// only when they are spread over all of them.
    Gathering(Arc<tokio::sync::Barrier>),
    /// Every session start with `SlowDown`, as a server does once a connection holds as many
    /// sessions as it allows.
    RefusingSessions,
    /// Every session start as the receiver reads.
    Controlling(tokio::sync::watch::Receiver<SessionStarts>),
    /// Every session start once the receiver reads `true`, having started the session at once.
    HoldingSessionStarts(tokio::sync::watch::Receiver<bool>),
    /// A session start presenting one of these authorizations with `NotAuthorized`.
    RefusingAuthorizations(Arc<[String]>),
}

impl Drop for TestStorageServer {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"test over");
    }
}

impl TestStorageServer {
    /// Serves on a loopback port, answering each request at once.
    pub fn start() -> Self {
        Self::serve(Answering::AtOnce)
    }

    /// Serves on a loopback port, holding each `get` until each of `connections` connections has
    /// one in flight.
    pub fn start_gathering(connections: usize) -> Self {
        Self::serve(Answering::Gathering(Arc::new(tokio::sync::Barrier::new(
            connections,
        ))))
    }

    /// Serves on a loopback port, answering every session start with `SlowDown`.
    pub fn start_refusing_sessions() -> Self {
        Self::serve(Answering::RefusingSessions)
    }

    /// Serves on a loopback port, refusing a session start that presents one of `authorizations`
    /// with `NotAuthorized`, as a server does once the token is revoked.
    pub fn start_refusing_authorizations(authorizations: &[&str]) -> Self {
        Self::serve(Answering::RefusingAuthorizations(
            authorizations
                .iter()
                .map(|token| token.to_string())
                .collect(),
        ))
    }

    /// Serves on a loopback port, answering every session start as the last value sent on the
    /// returned sender says, at first accepting it.
    pub fn start_controlling_session_starts() -> (Self, tokio::sync::watch::Sender<SessionStarts>) {
        let (control, controlled) = tokio::sync::watch::channel(SessionStarts::Accept);
        (Self::serve(Answering::Controlling(controlled)), control)
    }

    /// Serves on a loopback port, starting each session at once and holding its answer until
    /// `true` is sent on the returned sender.
    pub fn start_holding_session_starts() -> (Self, tokio::sync::watch::Sender<bool>) {
        let (release, released) = tokio::sync::watch::channel(false);
        (
            Self::serve(Answering::HoldingSessionStarts(released)),
            release,
        )
    }

    fn serve(answering: Answering) -> Self {
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
            .max_concurrent_bidi_streams(lore_transport::quic::client::STREAM_COUNT.into())
            .stream_receive_window(STREAM_WINDOW.into());

        let address: SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
        let endpoint = quinn::Endpoint::server(server_config, address).expect("server endpoint");
        let port = endpoint.local_addr().expect("bound address").port();
        let gets: Arc<parking_lot::Mutex<Vec<Arc<AtomicU64>>>> = Arc::default();
        let sessions: Arc<parking_lot::Mutex<Vec<Arc<parking_lot::Mutex<SessionMap>>>>> =
            Arc::default();
        let presented: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
        let reading = tokio::sync::watch::Sender::new(true);
        let fault: Arc<parking_lot::Mutex<Option<StreamFault>>> = Arc::default();
        let closed: Arc<AtomicUsize> = Arc::default();

        let accepting = endpoint.clone();
        let (accepted, accepted_sessions, logged, read, armed, ended) = (
            gets.clone(),
            sessions.clone(),
            presented.clone(),
            reading.subscribe(),
            fault.clone(),
            closed.clone(),
        );
        lore_spawn!(async move {
            while let Some(incoming) = accepting.accept().await {
                let Ok(connection) = incoming.await else {
                    continue;
                };
                let count = Arc::new(AtomicU64::new(0));
                accepted.lock().push(count.clone());
                let map = Arc::new(parking_lot::Mutex::new(SessionMap::default()));
                accepted_sessions.lock().push(map.clone());
                lore_spawn!(serve_connection(
                    connection,
                    Arc::new(Served {
                        gets: count,
                        serial: tokio::sync::Mutex::new(()),
                        sessions: map,
                        answering: answering.clone(),
                        presented: logged.clone(),
                        reading: read.clone(),
                        fault: armed.clone(),
                        closed: ended.clone(),
                    }),
                ));
            }
        });

        Self {
            url: format!("lore://127.0.0.1:{port}"),
            gets,
            sessions,
            presented,
            reading,
            fault,
            closed,
            endpoint,
        }
    }

    /// Leaves unread the payload of each request whose header arrives from now on, until
    /// [`resume_reading`](Self::resume_reading).
    pub fn pause_reading(&self) {
        self.reading.send_replace(false);
    }

    /// Reads the payloads [`pause_reading`](Self::pause_reading) left unread, and those after.
    pub fn resume_reading(&self) {
        self.reading.send_replace(true);
    }

    /// Connections accepted so far.
    pub fn accepted_connections(&self) -> usize {
        self.gets.lock().len()
    }

    /// Accepted connections that have closed so far.
    pub fn closed_connections(&self) -> usize {
        self.closed.load(Ordering::Relaxed)
    }

    /// The authorizations session starts presented so far, in order.
    pub fn presented_authorizations(&self) -> Vec<String> {
        self.presented.lock().clone()
    }

    /// Applies `fault` to the stream that answers the next request, leaving its connection up.
    pub fn arm_fault(&self, fault: StreamFault) {
        *self.fault.lock() = Some(fault);
    }

    /// Sessions started so far, per connection in the order they were accepted.
    pub fn started_sessions_per_connection(&self) -> Vec<u32> {
        self.sessions
            .lock()
            .iter()
            .map(|map| map.lock().next - 1)
            .collect()
    }

    /// Session starts asked for so far, answered or not, per connection in the order they were
    /// accepted.
    pub fn requested_session_starts_per_connection(&self) -> Vec<u32> {
        self.sessions
            .lock()
            .iter()
            .map(|map| map.lock().requested)
            .collect()
    }

    /// Sessions started and not yet stopped, per connection in the order they were accepted.
    pub fn live_sessions_per_connection(&self) -> Vec<usize> {
        self.sessions
            .lock()
            .iter()
            .map(|map| map.lock().live.len())
            .collect()
    }

    /// `get` requests received so far, served or not, per connection in the order they were
    /// accepted.
    pub fn received_gets_per_connection(&self) -> Vec<u64> {
        self.sessions
            .lock()
            .iter()
            .map(|map| map.lock().received)
            .collect()
    }

    /// The partition of the session each `get` ran under, per connection in the order they were
    /// accepted.
    pub fn served_partitions_per_connection(&self) -> Vec<Vec<Partition>> {
        self.sessions
            .lock()
            .iter()
            .map(|map| map.lock().served.clone())
            .collect()
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
            connected.push(self.connect_client().await);
        }
        connected
    }

    /// Opens one storage connection.
    pub async fn connect_client(&self) -> Arc<StorageClient> {
        StorageClient::connect(
            Weak::new(),
            &self.url,
            "127.0.0.1".to_string(),
            &AuthEndpoint::none(),
            "",
            test_partition(),
            &Arc::new(SuppliedCredentials::default()),
            None,
        )
        .await
        .expect("connect to the test server")
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

/// What the streams of one connection share: the `get`s it served, the lock that serves them one
/// at a time, its sessions, how it answers, and the server's record of authorizations presented,
/// whether it reads payloads, its armed fault and its count of closed connections.
struct Served {
    gets: Arc<AtomicU64>,
    serial: tokio::sync::Mutex<()>,
    sessions: Arc<parking_lot::Mutex<SessionMap>>,
    answering: Answering,
    presented: Arc<parking_lot::Mutex<Vec<String>>>,
    reading: tokio::sync::watch::Receiver<bool>,
    fault: Arc<parking_lot::Mutex<Option<StreamFault>>>,
    closed: Arc<AtomicUsize>,
}

async fn serve_connection(connection: quinn::Connection, served: Arc<Served>) {
    while let Ok((send, recv)) = connection.accept_bi().await {
        lore_spawn!(serve_stream(send, recv, served.clone()));
    }
    served.closed.fetch_add(1, Ordering::Relaxed);
}

async fn serve_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    served: Arc<Served>,
) {
    let Served {
        gets,
        serial,
        sessions,
        answering,
        presented,
        reading,
        fault,
        closed: _,
    } = &*served;
    let mut reading = reading.clone();
    let mut header_bytes = [0u8; COMMAND_HEADER_SIZE_V4];
    while recv.read_exact(&mut header_bytes).await.is_ok() {
        let header = CommandHeader::from_bytes_v4(&header_bytes);
        let mut request = vec![0u8; header.size_or_status as usize];
        if reading.wait_for(|reading| *reading).await.is_err()
            || recv.read_exact(&mut request).await.is_err()
        {
            return;
        }
        let response = match header.cmd {
            command if command == Command::Authorize as u8 && request.first() == Some(&0) => {
                serve_session_start(&request, sessions, presented, answering).await
            }
            command if command == Command::Authorize as u8 && request.first() == Some(&1) => {
                sessions
                    .lock()
                    .live
                    .retain(|&(live, _)| live != header.session_id);
                Ok(Bytes::new())
            }
            command if command == Command::Get as u8 => {
                serve_get(header.session_id, gets, serial, sessions, answering).await
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
        let fault = fault.lock().take();
        if matches!(fault, Some(StreamFault::StopReading)) {
            let _ = recv.stop(0u32.into());
        }
        if send.write_all_chunks(&mut chunks).await.is_err() {
            return;
        }
        if let Some(fault) = fault {
            return hold_faulted(fault, &header, send, recv).await;
        }
    }
}

/// Completes `fault` on a stream that has answered `header`, then holds both its halves without
/// end, so that neither closes.
async fn hold_faulted(
    fault: StreamFault,
    header: &CommandHeader,
    mut send: quinn::SendStream,
    _recv: quinn::RecvStream,
) {
    if matches!(fault, StreamFault::StrayAnswer) {
        let mut stray = header.response_success(0);
        stray.command_id = u32::MAX;
        let (stray, length) = stray.response_bytes();
        let _ = send.write_all(&stray[..length]).await;
    }
    std::future::pending::<()>().await;
}

/// The authorization a session start presents, after its action, partition, correlation id and
/// the authorization's length.
fn presented_authorization(request: &[u8]) -> String {
    let length_at = 18 + usize::from(request[17]);
    let length = usize::from(u16::from_le_bytes([
        request[length_at],
        request[length_at + 1],
    ]));
    String::from_utf8_lossy(&request[length_at + 2..][..length]).into_owned()
}

/// The id of a session `request` starts, for the partition it names, on the connection `sessions`
/// belongs to, answered as `answering` says: `SlowDown` where it refuses sessions, and
/// `NotAuthorized` where it refuses the authorization presented. `presented` records every
/// authorization a start presents.
async fn serve_session_start(
    request: &[u8],
    sessions: &parking_lot::Mutex<SessionMap>,
    presented: &parking_lot::Mutex<Vec<String>>,
    answering: &Answering,
) -> Result<Bytes, u32> {
    let authorization = presented_authorization(request);
    let refused = matches!(
        answering,
        Answering::RefusingAuthorizations(refused) if refused.contains(&authorization)
    );
    presented.lock().push(authorization);
    sessions.lock().requested += 1;
    let refusal = match answering {
        Answering::RefusingSessions => Some(QuicServiceError::SlowDown),
        Answering::Controlling(starts) => match starts
            .clone()
            .wait_for(|starts| *starts != SessionStarts::Hold)
            .await
            .map(|starts| *starts)
        {
            Ok(SessionStarts::Refuse) => Some(QuicServiceError::SlowDown),
            Ok(SessionStarts::RefuseAuthorization) => Some(QuicServiceError::NotAuthorized),
            _ => None,
        },
        _ if refused => Some(QuicServiceError::NotAuthorized),
        _ => None,
    };
    if let Some(refusal) = refusal {
        return Err(refusal as u32);
    }
    let partition: [u8; 16] = request[1..17].try_into().expect("a partition");
    let session_id = {
        let mut map = sessions.lock();
        let session_id = map.next;
        map.next += 1;
        map.live.push((session_id, Partition::from(partition)));
        session_id
    };
    if let Answering::HoldingSessionStarts(released) = answering {
        let _ = released.clone().wait_for(|released| *released).await;
    }
    Ok(Bytes::copy_from_slice(&session_id.to_le_bytes()))
}

/// A `get` on `session_id`, served one at a time per connection and held as the server gathers,
/// recording the session's partition, or `Failed` where the connection holds no such session.
async fn serve_get(
    session_id: u32,
    gets: &AtomicU64,
    serial: &tokio::sync::Mutex<()>,
    sessions: &parking_lot::Mutex<SessionMap>,
    answering: &Answering,
) -> Result<Bytes, u32> {
    {
        let mut map = sessions.lock();
        map.received += 1;
        let Some(&(_, partition)) = map.live.iter().find(|&&(live, _)| live == session_id) else {
            return Err(QuicServiceError::Failed as u32);
        };
        map.served.push(partition);
    }
    {
        let _serial = serial.lock().await;
        if let Answering::Gathering(gathering) = answering {
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
