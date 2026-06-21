// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::create_operation_context_attribute;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::PROTOCOL;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::SAMPLING_TIER_LOW;
use lore_telemetry::tracing::fields::TRANSPORT;
use lore_telemetry::tracing::fields::USER_ID;
use opentelemetry::KeyValue;
use opentelemetry_semantic_conventions::attribute::RPC_GRPC_STATUS_CODE;
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
use tracing::info_span;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::interpret_streaming_error;
use crate::grpc::log_server_error;
use crate::grpc::map_message_handle_error_to_status;
use crate::grpc::require_write_permission;
use crate::grpc::rpc_code_to_str;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::put::UnvalidatedPut;
use crate::protocol::storage::put::handle_put;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

pub type PutResponseStream =
    Pin<Box<dyn Stream<Item = Result<storage_v1::PutResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

#[tracing::instrument(name = "StorageServiceV1::Put", skip_all)]
pub async fn handler(
    request: Request<Streaming<storage_v1::PutRequest>>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<PutResponseStream>, Status> {
    let repository = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), repository)?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();

    let mut stream = request.into_inner();

    let (tx, rx) = mpsc::channel(super::STREAM_PROCESS_LIMIT);

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    let histogram = Arc::new(
        instrument_provider.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            lore_spawn!(async move {
                let task_limiter = Arc::new(Semaphore::new(super::STREAM_PROCESS_LIMIT));
                while let Some(req) = stream.next().await {
                    let permit = match Arc::clone(&task_limiter).acquire_owned().await {
                        Ok(p) => p,
                        Err(error) => {
                            debug!(?error, "Error acquiring put task permit");
                            break;
                        }
                    };

                    let immutable_store = immutable_store.clone();
                    let tx = tx.clone();
                    let correlation_id = correlation_id.clone();
                    let user_id = user_id.clone();
                    let histogram = histogram.clone();

                    let fragment_span = info_span!(
                        parent: None,
                        "StoragePutItemTask",
                        { SAMPLING_TIER_LOW } = true,
                        { TRANSPORT } = %Transport::Grpc,
                        { PROTOCOL } = %StorageProtocol::StorageV1,
                        { REPOSITORY_ID } = %repository,
                        { CORRELATION_ID } = correlation_id,
                        { USER_ID } = user_id,
                    );

                    lore_spawn!(
                        async move {
                            let start = Instant::now();
                            let metric_context = create_operation_context_attribute("put");

                            let put;
                            if let Err(stream_error) = req {
                                put = Err(interpret_streaming_error(stream_error));
                            }
                            else {
                                put = req.and_then(|r| {
                                    r.address
                                        .zip(r.fragment)
                                        .map(|(address, fragment)| UnvalidatedPut {
                                            address: address.into(),
                                            fragment: fragment.into(),
                                            payload: r.payload,
                                        })
                                        .ok_or(Status::invalid_argument(
                                            "Missing required field, both address and fragment must be present",
                                        ))
                                        .and_then(|unvalidated| {
                                            unvalidated.validate().map_err(|_e| {
                                                Status::invalid_argument("Payload failed validation")
                                            })
                                        })
                                });
                            }

                            let put_address = put.as_ref().ok().map(|p| *p.address());

                            let response = match put {
                                Ok(put) => {
                                    let address = *put.address();
                                    match handle_put(
                                        &put,
                                        repository,
                                        correlation_id,
                                        user_id,
                                        immutable_store,
                                    )
                                    .await
                                    {
                                        Ok(LoreResponse::Put(_)) => Ok(storage_v1::PutResponse {
                                            address: Some(address.into()),
                                        }),
                                        Ok(_) => Err(Status::internal(
                                            "Put handler returned the wrong response type",
                                        )),
                                        Err(err) => Err(
                                            map_message_handle_error_to_status(
                                                &err,
                                                Some(format!("Error storing fragment {address}: {err}")),
                                                None
                                            )
                                        ),
                                    }
                                }
                                Err(status) => Err(status),
                            };

                            let code = match &response {
                                Ok(_) => Code::Ok,
                                Err(status) => {
                                    log_server_error(status);
                                    status.code()
                                }
                            };
                            let elapsed_ms = start.elapsed().as_millis() as f64;
                            histogram.record(
                                elapsed_ms,
                                &[
                                    KeyValue::new(RPC_GRPC_STATUS_CODE, rpc_code_to_str(&code)),
                                    metric_context,
                                ],
                            );

                            if let Err(err) = tx.send(response).await {
                                debug!(address = ?put_address, "Error sending put response: {err}");
                            }
                            drop(permit);
                        }
                        .instrument(fragment_span)
                    );
                }
            });
        })
        .await;

    let recv_stream = ReceiverStream::from(rx);
    Ok(Response::new(Box::pin(recv_stream) as PutResponseStream))
}
