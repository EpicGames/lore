// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use async_trait::async_trait;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use crossbeam_epoch as epoch;
use lore_base::error::Disconnected;
use lore_base::lore_debug;
use lore_base::lore_trace;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_base::types::VerifyResult;
use lore_error_set::prelude::*;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use zerocopy::IntoBytes;

use super::super::QuicOpCode;
use super::super::client::AuthAdapter;
use super::super::client::BoundSession;
use super::super::client::DEFAULT_EXPECTED_RTT_MS;
use super::super::client::EndpointConfig;
use super::super::client::NO_GENERATION;
use super::super::client::QuicConnection;
use super::super::client::SendWithReconnectError;
use super::super::client::ServiceClient;
use super::super::client::TransportConfig;
use super::super::client::connect;
use super::super::client::send_high_priority_with_reconnect;
use super::super::client::send_normal;
use super::super::client::send_normal_with_reconnect;
use super::super::storage_service;
use super::super::storage_service::Command;
use super::super::storage_service::MAX_CHUNK_SIZE;
use super::super::storage_service::auth::StorageClientAuth;
use super::check_query_batch;
use super::check_query_statuses;
use super::read_fragment;
use super::read_hash;
use super::read_metadata;
use super::read_resolved;
use super::read_verify;
use super::sessions;
use super::sessions::Authorizer;
use super::sessions::IssueSession;
use super::sessions::Session;
use super::sessions::Sessions;
use super::sessions::session_start_payload;
use super::sessions::started_session_id;
use crate::auth::AuthEndpoint;
use crate::connection::Connection;
use crate::connection::SuppliedCredentials;
use crate::error::ProtocolError;
use crate::quic::client::CongestionAlgorithm;
use crate::traits::Storage;

const INFLIGHT_COMMAND_LIMIT: usize = 10000;

const MAX_BYTES_BANDWIDTH_PER_SEC: u64 = (1024 * 1024 * 1024) / 8;

#[allow(dead_code)]
pub struct StorageClient {
    connection: Weak<Connection>,
    remote_url: String,
    transport_config: TransportConfig,
    auth_adapter: Arc<dyn AuthAdapter<ErrorType = ProtocolError>>,
    authorizer: Authorizer,
    partition: Partition,
    counter: AtomicUsize,
    quic: Arc<QuicConnection>,
    connection_establish: Semaphore,
    command_limit: Semaphore,
    sent: AtomicUsize,
    sessions: Sessions,
    /// This client, for the task that issues a session again ([`sessions::rebind`]).
    this: Weak<StorageClient>,
}

impl Drop for StorageClient {
    fn drop(&mut self) {
        // Close the QUIC connection immediately without waiting for streams to
        // drain. Quinn sends a CLOSE frame and RSTs any open streams. The server
        // handles this gracefully (cleans up all sessions on connection close).
        // This avoids blocking the runtime during shutdown which would deadlock
        // with any in-flight session_stop sends on the same connection.
        self.quic.close_immediate();
        lore_trace!("QUIC storage client dropped");
    }
}

impl StorageClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connection: Weak<Connection>,
        remote_url: &str,
        transport_config: TransportConfig,
        auth_adapter: Arc<dyn AuthAdapter<ErrorType = ProtocolError>>,
        auth_endpoint: &AuthEndpoint,
        recipient_domain: &str,
        identity: &str,
        partition: Partition,
        quinn: quinn::Connection,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Arc<Self> {
        let quic = QuicConnection::with_v4(quinn, MAX_CHUNK_SIZE, true);
        Arc::new_cyclic(|this| StorageClient {
            connection,
            remote_url: remote_url.to_string(),
            transport_config,
            auth_adapter,
            authorizer: Authorizer::new(auth_endpoint, recipient_domain, identity, credentials),
            partition,
            quic: Arc::new(quic),
            connection_establish: Semaphore::new(1),
            counter: AtomicUsize::new(0),
            sent: AtomicUsize::new(0),
            command_limit: Semaphore::new(INFLIGHT_COMMAND_LIMIT),
            sessions: Sessions::default(),
            this: this.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        connection: Weak<Connection>,
        remote_url: &str,
        remote_domain: String,
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        partition: Partition,
        credentials: &Arc<SuppliedCredentials>,
        user_agent: Option<String>,
    ) -> Result<Arc<Self>, ProtocolError> {
        let user_agent = user_agent.unwrap_or_else(|| crate::user_agent().to_string());
        let auth_adapter = Arc::new(StorageClientAuth {
            recipient_domain: remote_domain.clone(),
            auth_url: auth_endpoint.url.clone(),
            identity: identity.to_string(),
            partition,
            user_agent,
        });
        let transport_config = TransportConfig {
            max_bytes_bandwidth_per_second: MAX_BYTES_BANDWIDTH_PER_SEC,
            expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
            congestion_algorithm: CongestionAlgorithm::Bbr,
            initial_cwnd: None,
        };

        lore_trace!("QUIC connecting to {remote_url} for partition {partition}");

        let start = Instant::now();

        let quinn = connect(
            &EndpointConfig {
                remote_url: remote_url.to_string(),
                default_port: Self::DEFAULT_PORT,
                sni_override: None,
            },
            auth_adapter.client_certs(),
            Self::ALPN,
            transport_config.clone(),
        )
        .await?;
        let connection_id = quinn.stable_id();

        let storage = StorageClient::new(
            connection,
            remote_url,
            transport_config,
            auth_adapter.clone(),
            auth_endpoint,
            &remote_domain,
            identity,
            partition,
            quinn,
            credentials,
        );

        lore_trace!(
            "QUIC connected to {remote_url} in {}ms",
            start.elapsed().as_millis()
        );

        storage
            .quic
            .create_initial_stream()
            .await
            .internal_with(|| {
                format!("creating initial QUIC stream to {remote_url} for partition {partition}")
            })?;

        auth_adapter.initial_authorize(storage.quic.clone()).await?;

        storage.quic.stream_count.store(1, Ordering::Relaxed);

        lore_debug!(
            "QUIC connection {connection_id} to {remote_url} for partition {partition} complete in {}ms",
            start.elapsed().as_millis()
        );

        Ok(storage)
    }

    /// The commands holding the last issue of the session `session_id` names: the handles to it
    /// beside the session's own.
    #[cfg(feature = "test-util")]
    pub fn awaiting_issue(&self, session_id: u32) -> usize {
        self.sessions.awaiting_issue(session_id)
    }
}

impl IssueSession for StorageClient {
    fn authorizer(&self) -> &Authorizer {
        &self.authorizer
    }

    fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    fn upgrade(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }

    fn is_live(&self, bound: BoundSession) -> bool {
        bound.generation == self.quic.generation()
    }

    async fn stop(&self, bound: BoundSession) {
        if !self.quic.has_streams().await {
            return;
        }
        let _ = send_normal(
            self.quic.clone(),
            Command::Authorize as QuicOpCode,
            bound,
            true,
            &mut [Bytes::default(), Bytes::from_static(&[1u8])],
        )
        .await;
    }

    async fn send_issue(&self, session: &Session, token: &str) -> Result<(), ProtocolError> {
        let payload = session_start_payload(session.partition, &session.correlation_id, token);
        let response = send_normal_with_reconnect(self, Command::Authorize, session.id, || {
            [Bytes::default(), payload.clone()]
        })
        .await?;
        session.bind(started_session_id(&response)?);
        Ok(())
    }
}

impl ServiceClient for StorageClient {
    type RequestType = Command;
    type ErrorType = ProtocolError;

    const ALPN: &'static str = "lore-storage/0.4";
    const DEFAULT_PORT: u16 = 41337;

    async fn acquire_command_permit(&self) -> Option<SemaphorePermit<'_>> {
        self.command_limit.acquire().await.ok()
    }

    fn quic(&self) -> &Arc<QuicConnection> {
        &self.quic
    }

    fn endpoint_config(&self) -> EndpointConfig {
        EndpointConfig {
            remote_url: self.remote_url.clone(),
            default_port: Self::DEFAULT_PORT,
            sni_override: None,
        }
    }

    fn alpn(&self) -> &str {
        Self::ALPN
    }

    fn map_send_error(
        &self,
        failed_request: Self::RequestType,
        error: SendWithReconnectError,
    ) -> Self::ErrorType {
        match error {
            SendWithReconnectError::PermitAcquire => ProtocolError::internal("permit acquire"),
            SendWithReconnectError::Disconnected => ProtocolError::from(Disconnected),
            SendWithReconnectError::ReconnectFailed => {
                if let Some(connection) = self.connection.upgrade() {
                    connection.stale.store(true, Ordering::Relaxed);
                }
                ProtocolError::from(Disconnected)
            }
            SendWithReconnectError::ClientError(client_error) => {
                storage_service::command_error(&failed_request, client_error)
            }
        }
    }

    fn auth_adapter(&self) -> &Arc<dyn AuthAdapter<ErrorType = Self::ErrorType>> {
        &self.auth_adapter
    }

    fn transport_config(&self) -> TransportConfig {
        self.transport_config.clone()
    }

    fn v4_protocol(&self) -> bool {
        true
    }

    /// An `Authorize` request issues the session it names on the current connection, and records
    /// that connection as the one the server's answer belongs to. Any other request carries the
    /// session as the server last issued it, or binds to no connection once it is stopped.
    fn bind_session(&self, request_type: Command, session_id: u32) -> BoundSession {
        let guard = epoch::pin();
        let session = self.sessions.find(session_id, &guard);
        if request_type == Command::Authorize {
            let generation = self.quic.generation();
            if let Some(session) = session {
                session.issue_on(generation);
            }
            return BoundSession { id: 0, generation };
        }
        session.map_or(
            BoundSession {
                id: 0,
                generation: NO_GENERATION,
            },
            |session| session.bound(),
        )
    }

    /// An `Authorize` request is bound afresh on its next attempt, as it issues the session.
    async fn rebind_session(
        &self,
        request_type: Command,
        session_id: u32,
    ) -> Result<(), ProtocolError> {
        if request_type == Command::Authorize {
            return Ok(());
        }
        sessions::rebind(self, session_id, self.quic.generation()).await
    }
}

#[async_trait]
impl Storage for StorageClient {
    async fn session_start(
        &self,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        sessions::start(self, partition, correlation_id).await
    }

    async fn session_stop(&self, session_id: u32) -> Result<(), ProtocolError> {
        sessions::stop(self, session_id).await
    }

    async fn get(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let mut payload = send_normal_with_reconnect(self, Command::Get, session_id, || {
            [Bytes::default(), Bytes::from_owner(*address)]
        })
        .await?;

        let fragment = read_fragment(&mut payload, "get")?;
        Ok((fragment, payload))
    }

    /// Response framing: resolved `Hash` (32) ++ `Fragment` (16) ++ payload.
    async fn get_resolved(
        &self,
        session_id: u32,
        key: &Hash,
        context: &Context,
        flags: u32,
    ) -> Result<(Hash, Fragment, Bytes), ProtocolError> {
        let tail = flags.to_le_bytes();

        let mut payload =
            send_normal_with_reconnect(self, Command::GetResolved, session_id, || {
                [
                    Bytes::default(),
                    Bytes::from_owner(*key),
                    Bytes::from_owner(*context),
                    Bytes::copy_from_slice(&tail),
                ]
            })
            .await?;

        let (resolved, fragment) = read_resolved(&mut payload)?;
        Ok((resolved, fragment, payload))
    }

    async fn get_metadata(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        let payload = send_normal_with_reconnect(self, Command::GetMetadata, session_id, || {
            [Bytes::default(), Bytes::from_owner(*address)]
        })
        .await?;

        read_metadata(&payload)
    }

    async fn get_priority(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let mut payload = send_high_priority_with_reconnect(self, Command::Get, session_id, || {
            [Bytes::default(), Bytes::from_owner(*address)]
        })
        .await?;

        let fragment = read_fragment(&mut payload, "get")?;
        Ok((fragment, payload))
    }

    async fn put(
        &self,
        session_id: u32,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        send_normal_with_reconnect(self, Command::Put, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(address),
                Bytes::from_owner(fragment),
                payload.clone().unwrap_or_default(),
            ]
        })
        .await
        .map(|_| ())
    }

    /// Request framing: `put`'s, with the mutable key prepended — key (32) ++ `Address` (48) ++
    /// `Fragment` (16) ++ payload, keeping the 96-byte header a multiple of four.
    async fn put_resolved(
        &self,
        session_id: u32,
        key: &Hash,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        send_normal_with_reconnect(self, Command::PutResolved, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(*key),
                Bytes::from_owner(address),
                Bytes::from_owner(fragment),
                payload.clone().unwrap_or_default(),
            ]
        })
        .await
        .map(|_| ())
    }

    async fn query(&self, session_id: u32, address: &[Address]) -> Result<Bytes, ProtocolError> {
        let address_count = address.len();
        check_query_batch(address_count)?;

        // Allocate a copy on the heap to avoid lifetime issues
        let address = Bytes::copy_from_slice(address.as_bytes());
        let payload = send_normal_with_reconnect(self, Command::Query, session_id, || {
            [Bytes::default(), address.clone()]
        })
        .await?;

        check_query_statuses(&payload, address_count)?;
        Ok(payload)
    }

    async fn verify(
        &self,
        session_id: u32,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        let heal_byte = if heal { 1u8 } else { 0u8 };
        let mut request_bytes = BytesMut::with_capacity(size_of::<Address>() + 1);
        request_bytes.extend_from_slice(address.as_bytes());
        request_bytes.put_u8(heal_byte);
        let request_bytes = request_bytes.freeze();

        let payload = send_normal_with_reconnect(self, Command::Verify, session_id, || {
            [Bytes::default(), request_bytes.clone()]
        })
        .await?;

        read_verify(&payload, address)
    }

    async fn copy(
        &self,
        session_id: u32,
        source_partition: Partition,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        // Wire payload layout for Copy is: source_partition (16) + source_address (32+16) +
        // target_context (16) = 80 bytes. The target_context tail allows the destination's
        // dedup tag to differ from the source's without transferring the payload.
        send_normal_with_reconnect(self, Command::Copy, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(source_partition),
                Bytes::from_owner(source_address),
                Bytes::from_owner(target_context),
            ]
        })
        .await
        .map(|_| ())
    }

    async fn mutable_load(
        &self,
        session_id: u32,
        key: &Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let payload = send_normal_with_reconnect(self, Command::MutableLoad, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(*key),
                Bytes::copy_from_slice(&[key_type as u8]),
            ]
        })
        .await?;

        read_hash(&payload, "mutable_load")
    }

    async fn mutable_store(
        &self,
        session_id: u32,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        send_normal_with_reconnect(self, Command::MutableStore, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(key),
                Bytes::from_owner(value),
                Bytes::copy_from_slice(&[key_type as u8]),
            ]
        })
        .await
        .map(|_| ())
    }

    async fn mutable_compare_and_swap(
        &self,
        session_id: u32,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let payload = send_normal_with_reconnect(self, Command::MutableCas, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(key),
                Bytes::from_owner(expected),
                Bytes::from_owner(value),
                Bytes::copy_from_slice(&[key_type as u8]),
            ]
        })
        .await?;

        read_hash(&payload, "mutable_cas")
    }

    async fn close(&self) {
        self.quic.close().await;
    }
}
