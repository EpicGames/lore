// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_proto::ReplicationPutRequest;
use lore_proto::rpc::replication_service_server::ReplicationService;
use lore_revision::runtime::execution_context;
use lore_storage::ImmutableStore;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::create_operation_context_attribute;
use opentelemetry::KeyValue;
use opentelemetry_semantic_conventions::attribute::RPC_GRPC_STATUS_CODE;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;
use tracing::Instrument;
use tracing::debug;
use tracing::instrument;
use tracing::warn;

use crate::grpc::extract_correlation_id;
use crate::grpc::send_err;
use crate::util::REPLICATION_USER_ID;
use crate::util::setup_execution;

type PutResponseStream =
    Pin<Box<dyn Stream<Item = Result<lore_proto::PutResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

#[derive(Clone, Debug, Error)]
pub enum ReplicationServiceError {
    #[error("Immutable store must be a local store")]
    NonLocalStore,
}

pub struct LoreReplicationService {
    immutable_store: Arc<dyn ImmutableStore>,
}

impl LoreReplicationService {
    pub fn new(immutable_store: Arc<dyn ImmutableStore>) -> Result<Self, ReplicationServiceError> {
        if !immutable_store.is_local() {
            return Err(ReplicationServiceError::NonLocalStore);
        }

        Ok(Self { immutable_store })
    }
}

#[tonic::async_trait]
impl ReplicationService for LoreReplicationService {
    type PutStream = PutResponseStream;

    #[instrument(name = "ReplicationService::put", skip_all)]
    async fn put(
        &self,
        request: Request<Streaming<ReplicationPutRequest>>,
    ) -> Result<Response<Self::PutStream>, Status> {
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let mut stream = request.into_inner();

        // TODO(jcohen): make this capacity configurable or unbounded
        let message_limit = 8192;
        let (tx, rx) = mpsc::channel(message_limit);

        let immutable_store = self.immutable_store.clone();

        let execution = setup_execution(
            module_path!(),
            correlation_id,
            REPLICATION_USER_ID.to_owned(),
        );
        let histogram =
            Arc::new(self.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY));

        // Spawn task to read from client
        lore_spawn!(
            LORE_CONTEXT.scope(
                execution,
                async move {
                    let task_limiter = Arc::new(Semaphore::new(message_limit));
                    while let Some(req) = stream.next().await {
                        if let Err(e) = req {
                            debug!(error = ?e, "Error reading message from stream");
                            break;
                        }

                        let immutable_store = immutable_store.clone();

                        let tx = tx.clone();

                        let histogram = histogram.clone();
                        let start = Instant::now();

                        let permit = match Arc::clone(&task_limiter).acquire_owned().await {
                            Ok(p) => p,
                            Err(error) => {
                                debug!(?error, "Error getting permit");
                                break;
                            }
                        };
                        lore_spawn!(LORE_CONTEXT.scope(execution_context(), async move {
                            let metric_context = create_operation_context_attribute("put");

                            // This is building up a tuple of (repository id, address, fragment, payload)
                            // out of the various optional and required pieces of the incoming request.
                            let request = req.and_then(|r| {
                                r.put_request
                            .and_then(|p| p.address.zip(p.fragment).map(|(a, f)| (a, f, p.payload)))
                            .ok_or(Status::invalid_argument(
                                "Missing required field, both address and fragment must be present",
                            ))
                            .map(|(address, fragment, payload)| {
                                (
                                    Into::<Context>::into(r.repository_id),
                                    Into::<Address>::into(address),
                                    Into::<Fragment>::into(fragment),
                                    payload,
                                )
                            })
                            });

                            if let Err(err) = request {
                                let elapsed_ms = start.elapsed().as_millis() as f64;
                                histogram.record(
                                    elapsed_ms,
                                    &[
                                        KeyValue::new(
                                            RPC_GRPC_STATUS_CODE,
                                            format!("{:?}", err.code()),
                                        ),
                                        metric_context,
                                    ],
                                );
                                return send_err(err, tx).await;
                            }

                            let (repository_id, address, fragment, payload) = request.unwrap();

                            // Note: we go directly to the (local) store here, this bypasses the checks we
                            // normally do when storing new fragments like hash validation. We're relying on
                            // the fact that these requests are coming from trusted callers who have already
                            // performed this validation.
                            let put_result = immutable_store
                                .put(
                                    repository_id.into(),
                                    address,
                                    fragment,
                                    payload,
                                    false, /* force */
                                )
                                .await;

                            let metrics_code = match &put_result {
                                Ok(_) => Code::Ok,
                                Err(error) => {
                                    warn!(?error, "Error performing put");
                                    Code::Internal
                                },
                            };
                            let elapsed_ms = start.elapsed().as_millis() as f64;
                            histogram.record(
                                elapsed_ms,
                                &[
                                    KeyValue::new(RPC_GRPC_STATUS_CODE, format!("{metrics_code:?}")),
                                    metric_context,
                                ],
                            );

                            // always respond to the request so that replication clients
                            // know the request was successfully received/handled - they don't need
                            // to know whether it was successfully put since replication is best
                            // effort
                            let response = lore_proto::PutResponse {
                                address: Some(address.into()),
                            };

                            if let Err(err) = tx.send(Ok(response)).await {
                                warn!(
                                    error = ?err,
                                    address = ?address,
                                    "Failed to send response for replication put"
                                );
                            }

                            debug!(%address, "Successfully replicated fragment to local store");
                            drop(permit);
                        }.in_current_span()));
                    }
                }
                .in_current_span(),
            ),
        );

        Ok(Response::new(
            Box::pin(ReceiverStream::from(rx)) as Self::PutStream
        ))
    }
}

impl InstrumentProvider for LoreReplicationService {
    fn namespace(&self) -> &'static str {
        "urc.grpc.replication_service"
    }
}
