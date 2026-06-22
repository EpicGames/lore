// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::AddressNotFound;
use lore_base::error::PayloadNotFound;
use lore_base::error::SlowDown;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_revision::lore_debug;
use lore_revision::runtime::execution_context;
use lore_storage::StoreError;
use lore_telemetry::LabelArray;
use lore_telemetry::observe::observe_result;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::PARTITION_ID;
use lore_transport::ProtocolError;
use lore_transport::quic::QuicClientError;
use lore_transport::quic::client::AuthAdapter;
use lore_transport::quic::client::CertificateSettings;
pub use lore_transport::quic::client::ConnectionStats;
use lore_transport::quic::client::EndpointConfig;
use lore_transport::quic::client::QuicConnection;
use lore_transport::quic::client::SendWithReconnectError;
use lore_transport::quic::client::ServiceClient;
use lore_transport::quic::client::TransportConfig;
use lore_transport::quic::client::connect;
use lore_transport::quic::client::send_normal_with_reconnect;
use opentelemetry::KeyValue;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tracing::trace;
use tracing::warn;

use crate::protocol::replication_store::exists_batch::ExistsBatch;
use crate::protocol::replication_store::exists_batch::ExistsBatchResponse;
use crate::protocol::replication_store::get::Get;
use crate::protocol::replication_store::get::GetResponse;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::replication_store::obliterate::Obliterate;
use crate::protocol::replication_store::obliterate::ObliterateResponse;
use crate::protocol::replication_store::put::Put;
use crate::protocol::replication_store::put::PutFlags;
use crate::protocol::replication_store::query::Query;
use crate::protocol::replication_store::query::QueryResponse;
use crate::quic::replication_store_service::Command;
use crate::quic::replication_store_service::MAX_CHUNK_SIZE;
use crate::quic::replication_store_service::ReplicationServiceErrorCode;

pub const DEFAULT_MAX_BYTES_BANDWIDTH_PER_SEC: u64 = (1024 * 1024 * 1024 * 10) / 8;

#[derive(Debug, Error, Clone)]
pub enum ReplicationStoreClientError {
    #[error("Client side request throttling is occurring")]
    ClientSideThrottling,
    #[error("Connection to the server has failed with no more recovery attempts")]
    ConnectionFailed,
    #[error("Server side message throttling is occurring")]
    ServerSideMessageThrottling,
    #[error("An unexpected quic error has occurred: {0}")]
    UnexpectedClientError(QuicClientError),
    #[error("Replication Service error: {0}")]
    ServiceError(ReplicationServiceErrorCode),
    #[error("Response received successfully, but error encountered: '{0}'")]
    ResponseError(&'static str),
}

impl From<ReplicationServiceErrorCode> for ReplicationStoreClientError {
    fn from(code: ReplicationServiceErrorCode) -> Self {
        Self::ServiceError(code)
    }
}

struct ReplicationStoreAuth {
    certs: CertificateSettings,
}

#[async_trait]
impl AuthAdapter for ReplicationStoreAuth {
    type ErrorType = ReplicationStoreClientError;

    async fn initial_authorize(
        &self,
        _connection: Arc<QuicConnection>,
    ) -> Result<(), Self::ErrorType> {
        Ok(())
    }

    async fn reconnect_authorize(
        &self,
        _connection: Arc<QuicConnection>,
    ) -> Result<(), QuicClientError> {
        Ok(())
    }

    fn client_certs(&self) -> CertificateSettings {
        self.certs.clone()
    }
}

/// The core functionality of a client interacting with Store Service
#[async_trait]
pub trait StoreClient: Send + Sync + Sized + 'static {
    /// Gets the underlying connection stats
    async fn connection_stats(&self) -> Option<ConnectionStats>;

    /// Request an Immutable `Put` on the server
    async fn put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;

    /// Request an Immutable `ExistsBatch` on the server
    async fn exists_batch(
        &self,
        request: ExistsBatch,
    ) -> Result<ExistsBatchResponse, ReplicationStoreClientError>;

    /// Request an Immutable `Obliterate` on the server
    async fn obliterate(
        &self,
        request: Obliterate,
    ) -> Result<ObliterateResponse, ReplicationStoreClientError>;

    /// Request an Immutable `Get` on the server
    async fn get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError>;

    /// Request an Immutable `Query` on the server
    async fn query(&self, request: Query) -> Result<QueryResponse, ReplicationStoreClientError>;

    /// Request an Immutable `Put` on the server's local store
    async fn local_put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;

    /// Request an Immutable `ExistsBatch` on the server's local store
    async fn local_exists_batch(
        &self,
        request: ExistsBatch,
    ) -> Result<ExistsBatchResponse, ReplicationStoreClientError>;

    /// Request an Immutable `Get` on the server's local store
    async fn local_get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError>;

    /// Request an Immutable `Query` on the server's local store
    async fn local_query(
        &self,
        request: Query,
    ) -> Result<QueryResponse, ReplicationStoreClientError>;
}

#[derive(Clone)]
pub struct CommandBehavior {
    pub message_limit: usize,
    pub should_await_command_permit: bool,
}

pub struct ReplicationStoreClient {
    remote_url: String,
    sni_override: Option<String>,
    auth: Arc<dyn AuthAdapter<ErrorType = ReplicationStoreClientError>>,
    transport_config: TransportConfig,
    quic: Arc<QuicConnection>,
    command_limit: Semaphore,
    should_await_command_permit: bool,
}

impl Drop for ReplicationStoreClient {
    fn drop(&mut self) {
        let runtime = runtime();
        if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
            // Only in tests, here we cannot block in place to call the async flush
            // Just ignore for now, until we actually need to flush on drop in tests
        } else {
            trace!("ReplicationStoreClient drop block on readers in place");
            tokio::task::block_in_place(move || {
                runtime.block_on(LORE_CONTEXT.scope(execution_context(), async move {
                    self.quic.close().await;
                }));
            });
        }

        trace!("ReplicationStoreClient dropped");
    }
}

impl ReplicationStoreClient {
    /// Connect to the Replication Storage Service.
    /// Outside local/test environments, the server uses mTLS so clients
    /// should set the custom CA file as well as their client credentials
    pub async fn connect(
        remote_url: &str,
        certs: CertificateSettings,
        sni_override: Option<String>,
        transport_config: TransportConfig,
        command_behavior: CommandBehavior,
        max_reconnects: Option<u32>,
    ) -> Result<Self, ProtocolError> {
        trace!("ReplicationStoreClient connecting to {remote_url}");

        let start = Instant::now();
        let auth = Arc::new(ReplicationStoreAuth { certs });

        let quinn = connect(
            &EndpointConfig {
                remote_url: remote_url.to_string(),
                default_port: Self::DEFAULT_PORT,
                sni_override: sni_override.clone(),
            },
            auth.client_certs(),
            Self::ALPN,
            transport_config.clone(),
        )
        .await?;
        let connection_id = quinn.stable_id();
        let mut quic = QuicConnection::new(quinn, MAX_CHUNK_SIZE);
        quic.set_max_reconnects(max_reconnects);

        let client = Self {
            remote_url: remote_url.to_string(),
            sni_override,
            auth,
            quic: Arc::new(quic),
            command_limit: Semaphore::new(command_behavior.message_limit),
            should_await_command_permit: command_behavior.should_await_command_permit,
            transport_config,
        };

        trace!(
            "ReplicationStoreClient connected to {remote_url} in {}ms",
            start.elapsed().as_millis()
        );

        client.quic.create_initial_stream().await.map_err(|e| {
            lore_debug!("ReplicationStoreClient connection {connection_id} to {remote_url} - error making initial stream: {e:?}");
            ProtocolError::internal(format!("connecting to {remote_url}"))
        }
        )?;
        client.quic.stream_count.store(1, Ordering::Relaxed);

        lore_debug!(
            "ReplicationStoreClient connection {connection_id} to {remote_url} complete in {}ms",
            start.elapsed().as_millis()
        );

        Ok(client)
    }

    async fn send_put(
        &self,
        request: Put,
        command: Command,
    ) -> Result<(), ReplicationStoreClientError> {
        let quic_chunks = request.to_quic_chunks();
        send_normal_with_reconnect(self, command, 0, || quic_chunks.clone()).await?;
        Ok(())
    }

    async fn send_exists_batch(
        &self,
        request: ExistsBatch,
        command: Command,
    ) -> Result<ExistsBatchResponse, ReplicationStoreClientError> {
        let num_input_addresses = request.addresses.len();
        let quic_chunks = request.to_quic_chunks();
        let response_bytes =
            send_normal_with_reconnect(self, command, 0, || quic_chunks.clone()).await?;
        let response = ExistsBatchResponse::parse(response_bytes)?;
        if num_input_addresses != response.matches.len() {
            return Err(ReplicationStoreClientError::ResponseError(
                "response length mismatch",
            ));
        }
        Ok(response)
    }

    async fn send_get(
        &self,
        request: Get,
        command: Command,
    ) -> Result<GetResponse, ReplicationStoreClientError> {
        let quic_chunks = request.to_quic_chunks();
        let response_chunks =
            send_normal_with_reconnect(self, command, 0, || quic_chunks.clone()).await?;
        GetResponse::parse(response_chunks)
    }

    async fn send_query(
        &self,
        request: Query,
        command: Command,
    ) -> Result<QueryResponse, ReplicationStoreClientError> {
        let quic_chunks = request.to_quic_chunks();
        let response_chunks =
            send_normal_with_reconnect(self, command, 0, || quic_chunks.clone()).await?;
        QueryResponse::parse(response_chunks)
    }
}

#[async_trait]
impl StoreClient for ReplicationStoreClient {
    async fn connection_stats(&self) -> Option<ConnectionStats> {
        Some(self.quic.connection_stats().await)
    }

    async fn put(&self, request: Put) -> Result<(), ReplicationStoreClientError> {
        self.send_put(request, Command::ImmutablePut).await
    }

    async fn exists_batch(
        &self,
        request: ExistsBatch,
    ) -> Result<ExistsBatchResponse, ReplicationStoreClientError> {
        self.send_exists_batch(request, Command::ImmutableExistBatch)
            .await
    }

    async fn obliterate(
        &self,
        request: Obliterate,
    ) -> Result<ObliterateResponse, ReplicationStoreClientError> {
        let quic_chunks = request.to_quic_chunks();
        let response_chunks =
            send_normal_with_reconnect(self, Command::ImmutableObliterate, 0, || {
                quic_chunks.clone()
            })
            .await?;
        Ok(ObliterateResponse::parse(response_chunks)?)
    }

    async fn get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError> {
        self.send_get(request, Command::ImmutableGet).await
    }

    async fn query(&self, request: Query) -> Result<QueryResponse, ReplicationStoreClientError> {
        self.send_query(request, Command::ImmutableQuery).await
    }

    async fn local_put(&self, request: Put) -> Result<(), ReplicationStoreClientError> {
        self.send_put(request, Command::ImmutableLocalPut).await
    }

    async fn local_exists_batch(
        &self,
        request: ExistsBatch,
    ) -> Result<ExistsBatchResponse, ReplicationStoreClientError> {
        self.send_exists_batch(request, Command::ImmutableLocalExistBatch)
            .await
    }

    async fn local_get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError> {
        self.send_get(request, Command::ImmutableLocalGet).await
    }

    async fn local_query(
        &self,
        request: Query,
    ) -> Result<QueryResponse, ReplicationStoreClientError> {
        self.send_query(request, Command::ImmutableLocalQuery).await
    }
}

impl ServiceClient for ReplicationStoreClient {
    type RequestType = Command;
    type ErrorType = ReplicationStoreClientError;

    const ALPN: &'static str = "urc/rs-1.0";
    const DEFAULT_PORT: u16 = 41340;

    async fn acquire_command_permit(&self) -> Option<SemaphorePermit<'_>> {
        if self.should_await_command_permit {
            self.command_limit.acquire().await.ok()
        } else {
            self.command_limit.try_acquire().ok()
        }
    }

    fn quic(&self) -> &Arc<QuicConnection> {
        &self.quic
    }

    fn endpoint_config(&self) -> EndpointConfig {
        EndpointConfig {
            remote_url: self.remote_url.clone(),
            default_port: Self::DEFAULT_PORT,
            sni_override: self.sni_override.clone(),
        }
    }

    fn alpn(&self) -> &str {
        Self::ALPN
    }

    fn map_send_error(
        &self,
        _failed_request: Self::RequestType,
        error: SendWithReconnectError,
    ) -> Self::ErrorType {
        match error {
            SendWithReconnectError::PermitAcquire => {
                ReplicationStoreClientError::ClientSideThrottling
            }
            SendWithReconnectError::Disconnected | SendWithReconnectError::ReconnectFailed => {
                ReplicationStoreClientError::ConnectionFailed
            }
            SendWithReconnectError::ClientError(quic_error) => match quic_error {
                QuicClientError::SlowDown => {
                    ReplicationStoreClientError::ServerSideMessageThrottling
                }
                QuicClientError::ServerError(v) => match v {
                    v if v == ReplicationServiceErrorCode::Internal as u32 => {
                        ReplicationServiceErrorCode::Internal.into()
                    }
                    v if v == ReplicationServiceErrorCode::AddressNotFound as u32 => {
                        ReplicationServiceErrorCode::AddressNotFound.into()
                    }
                    v if v == ReplicationServiceErrorCode::SlowDown as u32 => {
                        ReplicationServiceErrorCode::SlowDown.into()
                    }
                    v if v == ReplicationServiceErrorCode::PayloadNotFound as u32 => {
                        ReplicationServiceErrorCode::PayloadNotFound.into()
                    }
                    _ => ReplicationStoreClientError::UnexpectedClientError(quic_error),
                },
                _ => ReplicationStoreClientError::UnexpectedClientError(quic_error),
            },
        }
    }

    fn auth_adapter(&self) -> &Arc<dyn AuthAdapter<ErrorType = Self::ErrorType>> {
        &self.auth
    }

    fn transport_config(&self) -> TransportConfig {
        self.transport_config.clone()
    }
}

#[derive(Clone)]
pub struct ServiceRequestMeta {
    pub client_epoch: u64,
    // best effort try to recreate the store error based off the request that was made.
    // these fields may not apply to all kinds of requests
    pub address: Option<Address>,
}

pub fn make_put_message(
    partition: Partition,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
    force: bool,
) -> Result<Put, ReplicationStoreClientError> {
    // message sizes are strictly enforced via QUIC, so if this size breaks that assumption
    // then early out
    if let Some(payload) = &payload
        && payload.len() > FRAGMENT_SIZE_THRESHOLD
    {
        warn!({PARTITION_ID} = %partition, {ADDRESS} = %address, ?fragment, payload_length = payload.len(), "put message payload too large");
        return Err(ReplicationStoreClientError::UnexpectedClientError(
            QuicClientError::ClientMessageTooBig,
        ));
    }

    let context = execution_context();
    let flags = PutFlags { force };
    let request = Put {
        header: ReplicationHeader {
            correlation_id: uuid::Uuid::try_parse(context.globals().correlation_id.as_str())
                .unwrap_or_default(),
            repository: partition.into(),
        },
        address,
        fragment,
        flags: flags.into(),
        payload,
    };
    Ok(request)
}

/// Maps a `ReplicationStoreClientError` to a `StoreError`, handling all error variants
/// except `ConnectionFailed` which requires caller-specific handling
pub fn map_client_error_to_store_error(
    error: ReplicationStoreClientError,
    meta: &ServiceRequestMeta,
) -> StoreError {
    match error {
        ReplicationStoreClientError::ServerSideMessageThrottling
        | ReplicationStoreClientError::ClientSideThrottling => StoreError::from(SlowDown),
        ReplicationStoreClientError::UnexpectedClientError(unexpected_error) => {
            warn!(?unexpected_error, "unexpected quic error");
            StoreError::internal_with_context(unexpected_error, "unexpected quic error")
        }
        ReplicationStoreClientError::ResponseError(response_error) => {
            warn!(?response_error, "response error");
            StoreError::internal(response_error)
        }
        ReplicationStoreClientError::ServiceError(service_error) => match service_error {
            ReplicationServiceErrorCode::Internal => {
                StoreError::internal("replication service internal error")
            }
            ReplicationServiceErrorCode::AddressNotFound => {
                StoreError::from(AddressNotFound::from(meta.address.unwrap_or_default()))
            }
            ReplicationServiceErrorCode::SlowDown => StoreError::from(SlowDown),
            ReplicationServiceErrorCode::PayloadNotFound => {
                StoreError::from(PayloadNotFound::from(meta.address.unwrap_or_default().hash))
            }
            ReplicationServiceErrorCode::Oversized => {
                StoreError::from(lore_base::error::Oversized {
                    context: "remote replication rejected oversized fragment".to_string(),
                })
            }
        },
        ReplicationStoreClientError::ConnectionFailed => {
            // Callers must handle ConnectionFailed before calling this function.
            // If we get here, treat it as an internal error.
            StoreError::internal("connection failed")
        }
    }
}

pub fn observe_client_interaction<ResponseType>()
-> impl Fn(&Result<ResponseType, ReplicationStoreClientError>, &Duration, &mut LabelArray) + Copy {
    // observes whether the store request was successfully forwarded to the server
    // and the response was gracefully handled.
    move |result: &Result<ResponseType, ReplicationStoreClientError>,
          elapsed: &Duration,
          labels: &mut LabelArray| {
        // base observability
        observe_result(result, elapsed, labels);

        let handled_value: &'static str = match result {
            Ok(_) => "ok",
            Err(error) => match error {
                ReplicationStoreClientError::ClientSideThrottling => "client_side_throttling",
                ReplicationStoreClientError::ConnectionFailed => "connection_failed",
                ReplicationStoreClientError::ServerSideMessageThrottling => {
                    "server_side_message_throttling"
                }
                ReplicationStoreClientError::UnexpectedClientError(_) => "unexpected_quic_error",
                // a graceful handling of a service error. I.e. A StoreError was successfully parsed
                // and understood. Does not necessarily mean you should treat this kind of response
                // as a bad thing - some observers might want to know while others might not
                ReplicationStoreClientError::ServiceError(_) => "graceful_service_error",
                ReplicationStoreClientError::ResponseError(_) => "response_error",
            },
        };
        labels.push(KeyValue::new("handled_status", handled_value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_errors_map_to_slow_down() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ClientSideThrottling,
            &meta,
        );
        assert!(matches!(error, StoreError::SlowDown(_)));

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ServerSideMessageThrottling,
            &meta,
        );
        assert!(matches!(error, StoreError::SlowDown(_)));
    }

    #[test]
    fn service_error_address_not_found_maps_correctly() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::AddressNotFound),
            &meta,
        );
        assert!(matches!(error, StoreError::AddressNotFound(_)));
    }

    #[test]
    fn service_error_slow_down_maps_correctly() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::SlowDown),
            &meta,
        );
        assert!(matches!(error, StoreError::SlowDown(_)));
    }

    #[test]
    fn service_error_payload_not_found_maps_correctly() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::PayloadNotFound),
            &meta,
        );
        assert!(matches!(error, StoreError::PayloadNotFound(_)));
    }

    #[test]
    fn service_error_internal_maps_correctly() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::Internal),
            &meta,
        );
        assert!(matches!(error, StoreError::Internal(_)));
    }

    #[test]
    fn connection_failed_maps_to_internal() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error =
            map_client_error_to_store_error(ReplicationStoreClientError::ConnectionFailed, &meta);
        assert!(matches!(error, StoreError::Internal(_)));
    }

    #[test]
    fn make_put_message_rejects_oversized_payload() {
        let payload = Bytes::from(vec![0u8; FRAGMENT_SIZE_THRESHOLD + 1]);
        let result = make_put_message(
            Partition::default(),
            Address::default(),
            Fragment::default(),
            Some(payload),
            false,
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ReplicationStoreClientError::UnexpectedClientError(
                QuicClientError::ClientMessageTooBig
            )
        ));
    }

    #[test]
    fn response_error_maps_to_internal() {
        let meta = ServiceRequestMeta {
            client_epoch: 0,
            address: None,
        };

        let error = map_client_error_to_store_error(
            ReplicationStoreClientError::ResponseError("bad response"),
            &meta,
        );
        assert!(matches!(error, StoreError::Internal(_)));
    }
}
